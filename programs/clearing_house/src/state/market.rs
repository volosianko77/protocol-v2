use anchor_lang::prelude::*;
use num_integer::Roots;
use solana_program::msg;
use std::cmp::{max, min};
use switchboard_v2::decimal::SwitchboardDecimal;
use switchboard_v2::AggregatorAccountData;

use crate::error::{ClearingHouseResult, ErrorCode};
use crate::math::amm;
use crate::math::casting::{cast, cast_to_i128, cast_to_i64, cast_to_u128};
use crate::math::margin::MarginRequirementType;
use crate::math_error;
use crate::state::bank::{BankBalance, BankBalanceType};
use crate::state::oracle::{OraclePriceData, OracleSource};
use crate::{
    BANK_IMF_PRECISION, BANK_WEIGHT_PRECISION, BID_ASK_SPREAD_PRECISION, MARGIN_PRECISION,
    MARK_PRICE_PRECISION, MARK_PRICE_PRECISION_I128,
};

#[account(zero_copy)]
#[derive(Default)]
#[repr(packed)]
pub struct Market {
    pub market_index: u64,
    pub pubkey: Pubkey,
    pub initialized: bool,
    pub amm: AMM,
    pub base_asset_amount_long: i128,
    pub base_asset_amount_short: i128,
    pub open_interest: u128, // number of users in a position
    pub margin_ratio_initial: u32,
    pub margin_ratio_partial: u32,
    pub margin_ratio_maintenance: u32,
    pub next_fill_record_id: u64,
    pub next_funding_rate_record_id: u64,
    pub next_curve_record_id: u64,
    pub pnl_pool: PoolBalance,
    pub unsettled_profit: u128,
    pub unsettled_loss: u128,
    pub imf_factor: u128,
    pub unsettled_initial_asset_weight: u8,
    pub unsettled_maintenance_asset_weight: u8,

    // upgrade-ability
    pub padding0: u32,
    pub padding1: u128,
    pub padding2: u128,
    pub padding3: u128,
    pub padding4: u128,
}

impl Market {
    pub fn get_margin_ratio(
        &self,
        size: u128,
        margin_type: MarginRequirementType,
    ) -> ClearingHouseResult<u32> {
        let margin_ratio = match margin_type {
            MarginRequirementType::Initial => self.margin_ratio_initial,
            MarginRequirementType::Partial => self.margin_ratio_partial,
            MarginRequirementType::Maintenance => self.margin_ratio_maintenance,
        };

        let mut margin_requirement = self.margin_ratio_partial as u128;

        let margin_ratio_max = match margin_type {
            MarginRequirementType::Initial => MARGIN_PRECISION, // 1x leverage
            MarginRequirementType::Partial => MARGIN_PRECISION, // 1x leverage
            MarginRequirementType::Maintenance => MARGIN_PRECISION + MARGIN_PRECISION / 10, // 1.1x leverage
        };

        // construct an initial
        if self.imf_factor > 0 {
            let size_sqrt = ((size / 1000) + 1).nth_root(2); //1e13 -> 1e10 -> 1e5

            let margin_requirement_numer = margin_requirement
                .checked_sub(
                    margin_requirement
                        .checked_div(BANK_IMF_PRECISION / self.imf_factor)
                        .ok_or_else(math_error!())?,
                )
                .ok_or_else(math_error!())?;

            // increases
            let size_surplus_margin_requirement = margin_requirement_numer
                .checked_add(
                    size_sqrt // 1e5
                        .checked_mul(self.imf_factor)
                        .ok_or_else(math_error!())?
                        .checked_div(100_000 * BANK_IMF_PRECISION / MARGIN_PRECISION) // 1e5 * 1e2
                        .ok_or_else(math_error!())?,
                )
                .ok_or_else(math_error!())?;
            // result between margin_requirement (10-20x) and 10_000 (1x)
            margin_requirement = min(
                max(margin_requirement, size_surplus_margin_requirement),
                margin_ratio_max,
            );
        }

        Ok(max(margin_ratio, margin_requirement as u32))
    }

    pub fn get_unsettled_asset_weight(
        &self,
        unsettled_pnl: i128,
        margin_type: MarginRequirementType,
    ) -> ClearingHouseResult<u128> {
        // the asset weight for a position's unrealised pnl + unsettled pnl
        // in the margin system
        // > 0 (positive balance)
        // < 0 (negative balance) always has asset weight = 1
        let mut unrealised_asset_weight = 100; // 100 = 1 in BANK_WEIGHT_PRECISION (1e3)
        if unsettled_pnl > 0 {
            let asset_weight = if margin_type == MarginRequirementType::Initial {
                let size = unsettled_pnl
                    .checked_mul(MARK_PRICE_PRECISION_I128)
                    .ok_or_else(math_error!())?
                    .checked_div(max(MARK_PRICE_PRECISION_I128, self.amm.last_oracle_price))
                    .ok_or_else(math_error!())?
                    .unsigned_abs();

                // rought approx of size in unit of base amount
                let unsettled_initial_asset_weight_discounted = min(
                    self.unsettled_initial_asset_weight,
                    ((BANK_IMF_PRECISION + BANK_IMF_PRECISION / 10)
                        .checked_mul(BANK_WEIGHT_PRECISION)
                        .ok_or_else(math_error!())?
                        .checked_div(
                            BANK_IMF_PRECISION
                                .checked_add(
                                    (size + 1)
                                        .nth_root(2) // 1e3
                                        .checked_mul(self.imf_factor as u128)
                                        .ok_or_else(math_error!())?
                                        .checked_div(1_000) // 1e3
                                        .ok_or_else(math_error!())?,
                                )
                                .ok_or_else(math_error!())?,
                        )
                        .ok_or_else(math_error!())?) as u8,
                );
                unsettled_initial_asset_weight_discounted
            } else {
                match margin_type {
                    MarginRequirementType::Initial => self.unsettled_initial_asset_weight,
                    MarginRequirementType::Partial => self.unsettled_maintenance_asset_weight,
                    MarginRequirementType::Maintenance => self.unsettled_maintenance_asset_weight,
                }
            };

            unrealised_asset_weight = asset_weight as u128;
        }

        // always ensure asset weight <= 1
        Ok(min(BANK_WEIGHT_PRECISION, unrealised_asset_weight))
    }
}

#[zero_copy]
#[derive(Default)]
pub struct PoolBalance {
    pub balance: u128,
}

impl BankBalance for PoolBalance {
    fn balance_type(&self) -> &BankBalanceType {
        &BankBalanceType::Deposit
    }

    fn balance(&self) -> u128 {
        self.balance
    }

    fn increase_balance(&mut self, delta: u128) -> ClearingHouseResult {
        self.balance = self.balance.checked_add(delta).ok_or_else(math_error!())?;
        Ok(())
    }

    fn decrease_balance(&mut self, delta: u128) -> ClearingHouseResult {
        self.balance = self.balance.checked_sub(delta).ok_or_else(math_error!())?;
        Ok(())
    }

    fn update_balance_type(&mut self, _balance_type: BankBalanceType) -> ClearingHouseResult {
        Err(ErrorCode::CantUpdatePoolBalanceType)
    }
}

#[zero_copy]
#[derive(Default)]
#[repr(packed)]
pub struct AMM {
    // oracle
    pub oracle: Pubkey,
    pub oracle_source: OracleSource,
    pub last_oracle_price: i128,
    pub last_oracle_conf_pct: u64,
    pub last_oracle_delay: i64,
    pub last_oracle_normalised_price: i128,
    pub last_oracle_price_twap: i128,
    pub last_oracle_price_twap_ts: i64,
    pub last_oracle_mark_spread_pct: i128,

    pub base_asset_reserve: u128,
    pub quote_asset_reserve: u128,
    pub sqrt_k: u128,
    pub peg_multiplier: u128,

    pub terminal_quote_asset_reserve: u128,
    pub net_base_asset_amount: i128,
    pub quote_asset_amount_long: u128,
    pub quote_asset_amount_short: u128,

    // funding
    pub last_funding_rate: i128,
    pub last_funding_rate_ts: i64,
    pub funding_period: i64,
    pub cumulative_funding_rate_long: i128,
    pub cumulative_funding_rate_short: i128,
    pub cumulative_funding_rate_lp: i128,
    pub cumulative_repeg_rebate_long: u128,
    pub cumulative_repeg_rebate_short: u128,

    pub mark_std: u64,
    pub last_mark_price_twap: u128,
    pub last_mark_price_twap_ts: i64,

    // trade constraints
    pub minimum_quote_asset_trade_size: u128,
    pub base_asset_amount_step_size: u128,

    // market making
    pub base_spread: u16,
    pub long_spread: u128,
    pub short_spread: u128,
    pub max_spread: u32,
    pub ask_base_asset_reserve: u128,
    pub ask_quote_asset_reserve: u128,
    pub bid_base_asset_reserve: u128,
    pub bid_quote_asset_reserve: u128,

    pub last_bid_price_twap: u128,
    pub last_ask_price_twap: u128,

    pub long_intensity_count: u16,
    pub long_intensity_volume: u64,
    pub short_intensity_count: u16,
    pub short_intensity_volume: u64,
    pub curve_update_intensity: u8,

    // fee tracking
    pub total_fee: u128,
    pub total_mm_fee: u128,
    pub total_exchange_fee: u128,
    pub total_fee_minus_distributions: i128,
    pub total_fee_withdrawn: u128,
    pub net_revenue_since_last_funding: i64,
    pub fee_pool: PoolBalance,
    pub last_update_slot: u64,

    pub padding0: u16,
    pub padding1: u32,
    pub padding2: u128,
    pub padding3: u128,
}

impl AMM {
    pub fn mark_price(&self) -> ClearingHouseResult<u128> {
        amm::calculate_price(
            self.quote_asset_reserve,
            self.base_asset_reserve,
            self.peg_multiplier,
        )
    }

    pub fn bid_price(&self, mark_price: u128) -> ClearingHouseResult<u128> {
        let bid_price = mark_price
            .checked_mul(
                BID_ASK_SPREAD_PRECISION
                    .checked_sub(self.short_spread)
                    .ok_or_else(math_error!())?,
            )
            .ok_or_else(math_error!())?
            .checked_div(BID_ASK_SPREAD_PRECISION)
            .ok_or_else(math_error!())?;

        Ok(bid_price)
    }

    pub fn ask_price(&self, mark_price: u128) -> ClearingHouseResult<u128> {
        let ask_price = mark_price
            .checked_mul(
                BID_ASK_SPREAD_PRECISION
                    .checked_add(self.long_spread)
                    .ok_or_else(math_error!())?,
            )
            .ok_or_else(math_error!())?
            .checked_div(BID_ASK_SPREAD_PRECISION)
            .ok_or_else(math_error!())?;

        Ok(ask_price)
    }

    pub fn bid_ask_price(&self, mark_price: u128) -> ClearingHouseResult<(u128, u128)> {
        let bid_price = self.bid_price(mark_price)?;
        let ask_price = self.ask_price(mark_price)?;
        Ok((bid_price, ask_price))
    }

    pub fn get_oracle_price(
        &self,
        price_oracle: &AccountInfo,
        clock_slot: u64,
    ) -> ClearingHouseResult<OraclePriceData> {
        match self.oracle_source {
            OracleSource::Pyth => self.get_pyth_price(price_oracle, clock_slot),
            OracleSource::Switchboard => self.get_switchboard_price(price_oracle, clock_slot),
            OracleSource::QuoteAsset => panic!(),
        }
    }

    pub fn can_lower_k(&self) -> ClearingHouseResult<bool> {
        let can_lower = self.net_base_asset_amount.unsigned_abs() < self.sqrt_k / 4;
        Ok(can_lower)
    }

    pub fn get_pyth_price(
        &self,
        price_oracle: &AccountInfo,
        clock_slot: u64,
    ) -> ClearingHouseResult<OraclePriceData> {
        let pyth_price_data = price_oracle
            .try_borrow_data()
            .or(Err(ErrorCode::UnableToLoadOracle))?;
        let price_data = pyth_client::cast::<pyth_client::Price>(&pyth_price_data);

        let oracle_price = cast_to_i128(price_data.agg.price)?;
        let oracle_conf = cast_to_u128(price_data.agg.conf)?;

        let oracle_precision = 10_u128.pow(price_data.expo.unsigned_abs());

        let mut oracle_scale_mult = 1;
        let mut oracle_scale_div = 1;

        if oracle_precision > MARK_PRICE_PRECISION {
            oracle_scale_div = oracle_precision
                .checked_div(MARK_PRICE_PRECISION)
                .ok_or_else(math_error!())?;
        } else {
            oracle_scale_mult = MARK_PRICE_PRECISION
                .checked_div(oracle_precision)
                .ok_or_else(math_error!())?;
        }

        let oracle_price_scaled = (oracle_price)
            .checked_mul(cast(oracle_scale_mult)?)
            .ok_or_else(math_error!())?
            .checked_div(cast(oracle_scale_div)?)
            .ok_or_else(math_error!())?;

        let oracle_conf_scaled = (oracle_conf)
            .checked_mul(oracle_scale_mult)
            .ok_or_else(math_error!())?
            .checked_div(oracle_scale_div)
            .ok_or_else(math_error!())?;

        let oracle_delay: i64 = cast_to_i64(clock_slot)?
            .checked_sub(cast(price_data.valid_slot)?)
            .ok_or_else(math_error!())?;

        Ok(OraclePriceData {
            price: oracle_price_scaled,
            confidence: oracle_conf_scaled,
            delay: oracle_delay,
            has_sufficient_number_of_data_points: true,
        })
    }

    pub fn get_switchboard_price(
        &self,
        price_oracle: &AccountInfo,
        clock_slot: u64,
    ) -> ClearingHouseResult<OraclePriceData> {
        let aggregator_data =
            AggregatorAccountData::new(price_oracle).or(Err(ErrorCode::UnableToLoadOracle))?;

        let price = convert_switchboard_decimal(&aggregator_data.latest_confirmed_round.result)?;
        let confidence =
            convert_switchboard_decimal(&aggregator_data.latest_confirmed_round.std_deviation)?;

        // std deviation should always be positive, if we get a negative make it u128::MAX so it's flagged as bad value
        let confidence = if confidence < 0 {
            u128::MAX
        } else {
            let price_10bps = price
                .unsigned_abs()
                .checked_div(1000)
                .ok_or_else(math_error!())?;
            max(confidence.unsigned_abs(), price_10bps)
        };

        let delay: i64 = cast_to_i64(clock_slot)?
            .checked_sub(cast(
                aggregator_data.latest_confirmed_round.round_open_slot,
            )?)
            .ok_or_else(math_error!())?;

        let has_sufficient_number_of_data_points =
            aggregator_data.latest_confirmed_round.num_success
                >= aggregator_data.min_oracle_results;

        Ok(OraclePriceData {
            price,
            confidence,
            delay,
            has_sufficient_number_of_data_points,
        })
    }

    pub fn get_oracle_twap(&self, price_oracle: &AccountInfo) -> ClearingHouseResult<Option<i128>> {
        match self.oracle_source {
            OracleSource::Pyth => Ok(Some(self.get_pyth_twap(price_oracle)?)),
            OracleSource::Switchboard => Ok(None),
            OracleSource::QuoteAsset => panic!(),
        }
    }

    pub fn get_pyth_twap(&self, price_oracle: &AccountInfo) -> ClearingHouseResult<i128> {
        let pyth_price_data = price_oracle
            .try_borrow_data()
            .or(Err(ErrorCode::UnableToLoadOracle))?;
        let price_data = pyth_client::cast::<pyth_client::Price>(&pyth_price_data);

        let oracle_twap = cast_to_i128(price_data.twap.val)?;

        let oracle_precision = 10_u128.pow(price_data.expo.unsigned_abs());

        let mut oracle_scale_mult = 1;
        let mut oracle_scale_div = 1;

        if oracle_precision > MARK_PRICE_PRECISION {
            oracle_scale_div = oracle_precision
                .checked_div(MARK_PRICE_PRECISION)
                .ok_or_else(math_error!())?;
        } else {
            oracle_scale_mult = MARK_PRICE_PRECISION
                .checked_div(oracle_precision)
                .ok_or_else(math_error!())?;
        }

        let oracle_twap_scaled = (oracle_twap)
            .checked_mul(cast(oracle_scale_mult)?)
            .ok_or_else(math_error!())?
            .checked_div(cast(oracle_scale_div)?)
            .ok_or_else(math_error!())?;

        Ok(oracle_twap_scaled)
    }
}

/// Given a decimal number represented as a mantissa (the digits) plus an
/// original_precision (10.pow(some number of decimals)), scale the
/// mantissa/digits to make sense with a new_precision.
fn convert_switchboard_decimal(
    switchboard_decimal: &SwitchboardDecimal,
) -> ClearingHouseResult<i128> {
    let switchboard_precision = 10_u128.pow(switchboard_decimal.scale);
    if switchboard_precision > MARK_PRICE_PRECISION {
        switchboard_decimal
            .mantissa
            .checked_div((switchboard_precision / MARK_PRICE_PRECISION) as i128)
            .ok_or_else(math_error!())
    } else {
        switchboard_decimal
            .mantissa
            .checked_mul((MARK_PRICE_PRECISION / switchboard_precision) as i128)
            .ok_or_else(math_error!())
    }
}
