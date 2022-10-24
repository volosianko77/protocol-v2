use anchor_lang::prelude::*;

use std::cmp::max;

use crate::controller::position::PositionDirection;
use crate::error::{ClearingHouseResult, ErrorCode};
use crate::math::amm;
use crate::math::casting::Cast;
use crate::math::constants::{
    AMM_RESERVE_PRECISION, BID_ASK_SPREAD_PRECISION_U128, MARGIN_PRECISION_U128,
    PRICE_PRECISION_I64, SPOT_WEIGHT_PRECISION, TWENTY_FOUR_HOUR,
};
use crate::math::margin::{
    calculate_size_discount_asset_weight, calculate_size_premium_liability_weight,
    MarginRequirementType,
};
use crate::math::safe_math::SafeMath;
use crate::math::stats;

use crate::state::oracle::{HistoricalOracleData, OracleSource};
use crate::state::spot_market::{SpotBalance, SpotBalanceType};
use crate::{AMM_TO_QUOTE_PRECISION_RATIO, MAX_CONCENTRATION_COEFFICIENT, PRICE_PRECISION};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq, Debug, Eq)]
pub enum MarketStatus {
    Initialized,    // warm up period for initialization, fills are paused
    Active,         // all operations allowed
    FundingPaused,  // perp: pause funding rate updates | spot: pause interest updates
    AmmPaused,      // amm fills are prevented/blocked
    FillPaused,     // fills are blocked
    WithdrawPaused, // perp: pause settling positive pnl | spot: pause withdrawing asset
    ReduceOnly,     // fills only able to reduce liability
    Settlement, // market has determined settlement price and positions are expired must be settled
    Delisted,   // market has no remaining participants
}

impl Default for MarketStatus {
    fn default() -> Self {
        MarketStatus::Initialized
    }
}

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq, Debug, Eq)]
pub enum ContractType {
    Perpetual,
    Future,
}

impl Default for ContractType {
    fn default() -> Self {
        ContractType::Perpetual
    }
}

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq, Debug, Eq)]
pub enum ContractTier {
    A,           // max insurance capped at A level
    B,           // max insurance capped at B level
    C,           // max insurance capped at C level
    Speculative, // no insurance
    Isolated,    // no insurance, only single position allowed
}

impl Default for ContractTier {
    fn default() -> Self {
        ContractTier::Speculative
    }
}

#[account(zero_copy)]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct PerpMarket {
    pub pubkey: Pubkey,
    pub amm: AMM,
    pub pnl_pool: PoolBalance,
    pub name: [u8; 32], // 256 bits
    pub insurance_claim: InsuranceClaim,
    pub unrealized_pnl_max_imbalance: u64,
    pub expiry_ts: i64,    // iff market in reduce only mode
    pub expiry_price: i64, // iff market has expired, price users can settle position
    pub next_fill_record_id: u64,
    pub next_funding_rate_record_id: u64,
    pub next_curve_record_id: u64,
    pub imf_factor: u32,
    pub unrealized_pnl_imf_factor: u32,
    pub liquidator_fee: u32,
    pub if_liquidation_fee: u32,
    pub margin_ratio_initial: u32,
    pub margin_ratio_maintenance: u32,
    pub unrealized_pnl_initial_asset_weight: u32,
    pub unrealized_pnl_maintenance_asset_weight: u32,
    pub number_of_users: u32, // number of users in a position
    pub market_index: u16,
    pub status: MarketStatus,
    pub contract_type: ContractType,
    pub contract_tier: ContractTier,
    pub padding: [u8; 7],
}

impl PerpMarket {
    pub fn is_active(&self, now: i64) -> ClearingHouseResult<bool> {
        let status_ok = !matches!(
            self.status,
            MarketStatus::Settlement | MarketStatus::Delisted
        );
        let not_expired = self.expiry_ts == 0 || now < self.expiry_ts;
        Ok(status_ok && not_expired)
    }

    pub fn is_reduce_only(&self) -> ClearingHouseResult<bool> {
        Ok(self.status == MarketStatus::ReduceOnly)
    }

    pub fn get_sanitize_clamp_denominator(self) -> ClearingHouseResult<Option<i64>> {
        Ok(match self.contract_tier {
            ContractTier::A => Some(10_i64),   // 10%
            ContractTier::B => Some(5_i64),    // 20%
            ContractTier::C => Some(2_i64),    // 50%
            ContractTier::Speculative => None, // DEFAULT_MAX_TWAP_UPDATE_PRICE_BAND_DENOMINATOR
            ContractTier::Isolated => None,    // DEFAULT_MAX_TWAP_UPDATE_PRICE_BAND_DENOMINATOR
        })
    }

    pub fn get_margin_ratio(
        &self,
        size: u128,
        margin_type: MarginRequirementType,
    ) -> ClearingHouseResult<u32> {
        if self.status == MarketStatus::Settlement {
            return Ok(0); // no liability weight on size
        }

        let default_margin_ratio = match margin_type {
            MarginRequirementType::Initial => self.margin_ratio_initial,
            MarginRequirementType::Maintenance => self.margin_ratio_maintenance,
        };

        let size_adj_margin_ratio = calculate_size_premium_liability_weight(
            size,
            self.imf_factor,
            default_margin_ratio,
            MARGIN_PRECISION_U128,
        )?;

        let margin_ratio = default_margin_ratio.max(size_adj_margin_ratio);

        Ok(margin_ratio)
    }

    pub fn get_initial_leverage_ratio(&self, margin_type: MarginRequirementType) -> u128 {
        match margin_type {
            MarginRequirementType::Initial => {
                MARGIN_PRECISION_U128 * MARGIN_PRECISION_U128 / self.margin_ratio_initial as u128
            }
            MarginRequirementType::Maintenance => {
                MARGIN_PRECISION_U128 * MARGIN_PRECISION_U128
                    / self.margin_ratio_maintenance as u128
            }
        }
    }

    pub fn default_test() -> Self {
        let amm = AMM::default_test();
        PerpMarket {
            amm,
            margin_ratio_initial: 1000,
            margin_ratio_maintenance: 500,
            ..PerpMarket::default()
        }
    }

    pub fn default_btc_test() -> Self {
        let amm = AMM::default_btc_test();
        PerpMarket {
            amm,
            margin_ratio_initial: 1000,    // 10x
            margin_ratio_maintenance: 500, // 5x
            status: MarketStatus::Initialized,
            ..PerpMarket::default()
        }
    }

    pub fn get_unrealized_asset_weight(
        &self,
        unrealized_pnl: i128,
        margin_type: MarginRequirementType,
    ) -> ClearingHouseResult<u32> {
        let mut margin_asset_weight = match margin_type {
            MarginRequirementType::Initial => self.unrealized_pnl_initial_asset_weight,
            MarginRequirementType::Maintenance => self.unrealized_pnl_maintenance_asset_weight,
        };

        if margin_type == MarginRequirementType::Initial && self.unrealized_pnl_max_imbalance > 0 {
            let net_unsettled_pnl = amm::calculate_net_user_pnl(
                &self.amm,
                self.amm.historical_oracle_data.last_oracle_price,
            )?;

            if net_unsettled_pnl > self.unrealized_pnl_max_imbalance.cast::<i128>()? {
                margin_asset_weight = margin_asset_weight
                    .cast::<u128>()?
                    .safe_mul(self.unrealized_pnl_max_imbalance.cast()?)?
                    .safe_div(net_unsettled_pnl.unsigned_abs())?
                    .cast()?;
            }
        }

        // the asset weight for a position's unrealized pnl + unsettled pnl in the margin system
        // > 0 (positive balance)
        // < 0 (negative balance) always has asset weight = 1
        let unrealized_asset_weight = if unrealized_pnl > 0 {
            // todo: only discount the initial margin s.t. no one gets liquidated over upnl?

            // a larger imf factor -> lower asset weight
            match margin_type {
                MarginRequirementType::Initial => calculate_size_discount_asset_weight(
                    unrealized_pnl
                        .unsigned_abs()
                        .safe_mul(AMM_TO_QUOTE_PRECISION_RATIO)?,
                    self.unrealized_pnl_imf_factor,
                    margin_asset_weight,
                )?,
                MarginRequirementType::Maintenance => self.unrealized_pnl_maintenance_asset_weight,
            }
        } else {
            SPOT_WEIGHT_PRECISION
        };

        Ok(unrealized_asset_weight)
    }

    pub fn get_open_interest(&self) -> u128 {
        self.amm
            .base_asset_amount_long
            .abs()
            .max(self.amm.base_asset_amount_short.abs())
            .unsigned_abs()
    }
}

#[zero_copy]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct InsuranceClaim {
    pub revenue_withdraw_since_last_settle: u64,
    pub max_revenue_withdraw_per_period: u64,
    pub quote_max_insurance: u64,
    pub quote_settled_insurance: u64,
    pub last_revenue_withdraw_ts: i64,
}

#[zero_copy]
#[derive(Default, Eq, PartialEq, Debug)]
#[repr(C)]
pub struct PoolBalance {
    pub scaled_balance: u128,
    pub market_index: u16,
    pub padding: [u8; 6],
}

impl SpotBalance for PoolBalance {
    fn market_index(&self) -> u16 {
        self.market_index
    }

    fn balance_type(&self) -> &SpotBalanceType {
        &SpotBalanceType::Deposit
    }

    fn balance(&self) -> u128 {
        self.scaled_balance
    }

    fn increase_balance(&mut self, delta: u128) -> ClearingHouseResult {
        self.scaled_balance = self.scaled_balance.safe_add(delta)?;
        Ok(())
    }

    fn decrease_balance(&mut self, delta: u128) -> ClearingHouseResult {
        self.scaled_balance = self.scaled_balance.safe_sub(delta)?;
        Ok(())
    }

    fn update_balance_type(&mut self, _balance_type: SpotBalanceType) -> ClearingHouseResult {
        Err(ErrorCode::CantUpdatePoolBalanceType)
    }
}

#[zero_copy]
#[derive(Default, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct AMM {
    pub oracle: Pubkey,
    pub historical_oracle_data: HistoricalOracleData,
    pub base_asset_amount_per_lp: i128,
    pub quote_asset_amount_per_lp: i128,
    pub fee_pool: PoolBalance,
    pub base_asset_reserve: u128,
    pub quote_asset_reserve: u128,
    pub concentration_coef: u128,
    pub min_base_asset_reserve: u128,
    pub max_base_asset_reserve: u128,
    pub sqrt_k: u128,
    pub peg_multiplier: u128,
    pub terminal_quote_asset_reserve: u128,
    pub base_asset_amount_long: i128,
    pub base_asset_amount_short: i128,
    pub base_asset_amount_with_amm: i128,
    pub base_asset_amount_with_unsettled_lp: i128,
    pub max_open_interest: u128,
    pub quote_asset_amount_long: i128,
    pub quote_asset_amount_short: i128,
    pub quote_entry_amount_long: i128,
    pub quote_entry_amount_short: i128,
    pub user_lp_shares: u128,
    pub last_funding_rate: i64,
    pub last_funding_rate_long: i64,
    pub last_funding_rate_short: i64,
    pub last_24h_avg_funding_rate: i64,
    pub total_fee: i128,
    pub total_mm_fee: i128,
    pub total_exchange_fee: u128,
    pub total_fee_minus_distributions: i128,
    pub total_fee_withdrawn: u128,
    pub total_liquidation_fee: u128,
    pub cumulative_funding_rate_long: i128,
    pub cumulative_funding_rate_short: i128,
    pub cumulative_social_loss: i128,
    pub ask_base_asset_reserve: u128,
    pub ask_quote_asset_reserve: u128,
    pub bid_base_asset_reserve: u128,
    pub bid_quote_asset_reserve: u128,
    pub last_oracle_normalised_price: i64,
    pub last_oracle_reserve_price_spread_pct: i64,
    pub last_bid_price_twap: u64,
    pub last_ask_price_twap: u64,
    pub last_mark_price_twap: u64,
    pub last_mark_price_twap_5min: u64,
    pub last_update_slot: u64,
    pub last_oracle_conf_pct: u64,
    pub net_revenue_since_last_funding: i64,
    pub last_funding_rate_ts: i64,
    pub funding_period: i64,
    pub order_step_size: u64,
    pub order_tick_size: u64,
    pub min_order_size: u64,
    pub max_position_size: u64,
    pub volume_24h: u64,
    pub long_intensity_volume: u64,
    pub short_intensity_volume: u64,
    pub last_trade_ts: i64,
    pub mark_std: u64,
    pub last_mark_price_twap_ts: i64,
    pub base_spread: u32,
    pub max_spread: u32,
    pub long_spread: u32,
    pub short_spread: u32,
    pub long_intensity_count: u32,
    pub short_intensity_count: u32,
    pub max_fill_reserve_fraction: u16,
    pub max_slippage_ratio: u16,
    pub curve_update_intensity: u8,
    pub amm_jit_intensity: u8,
    pub oracle_source: OracleSource,
    pub last_oracle_valid: bool,
}

impl AMM {
    pub fn default_test() -> Self {
        let default_reserves = 100 * AMM_RESERVE_PRECISION;
        // make sure tests dont have the default sqrt_k = 0
        AMM {
            base_asset_reserve: default_reserves,
            quote_asset_reserve: default_reserves,
            sqrt_k: default_reserves,
            concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
            order_step_size: 1,
            order_tick_size: 1,
            max_base_asset_reserve: u64::MAX as u128,
            min_base_asset_reserve: 0,
            terminal_quote_asset_reserve: default_reserves,
            peg_multiplier: crate::math::constants::PEG_PRECISION,
            max_spread: 1000,
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: PRICE_PRECISION_I64,
                ..HistoricalOracleData::default()
            },
            last_oracle_valid: true,
            ..AMM::default()
        }
    }

    pub fn default_btc_test() -> Self {
        AMM {
            base_asset_reserve: 65 * AMM_RESERVE_PRECISION,
            quote_asset_reserve: 63015384615,
            terminal_quote_asset_reserve: 64 * AMM_RESERVE_PRECISION,
            sqrt_k: 64 * AMM_RESERVE_PRECISION,

            peg_multiplier: 19_400_000_000,

            concentration_coef: MAX_CONCENTRATION_COEFFICIENT,
            max_base_asset_reserve: 90 * AMM_RESERVE_PRECISION,
            min_base_asset_reserve: 45 * AMM_RESERVE_PRECISION,

            base_asset_amount_with_amm: -(AMM_RESERVE_PRECISION as i128),
            mark_std: PRICE_PRECISION as u64,

            quote_asset_amount_long: 0,
            quote_asset_amount_short: 19_000_000_000, // short 1 BTC @ $19000
            historical_oracle_data: HistoricalOracleData {
                last_oracle_price: 19_400 * PRICE_PRECISION_I64,
                last_oracle_price_twap: 19_400 * PRICE_PRECISION_I64,
                last_oracle_price_twap_ts: 1662800000_i64,
                ..HistoricalOracleData::default()
            },
            last_mark_price_twap_ts: 1662800000,

            curve_update_intensity: 100,

            base_spread: 250,
            max_spread: 975,

            last_oracle_valid: true,
            ..AMM::default()
        }
    }

    pub fn amm_jit_is_active(&self) -> bool {
        self.amm_jit_intensity > 0
    }

    pub fn reserve_price(&self) -> ClearingHouseResult<u64> {
        amm::calculate_price(
            self.quote_asset_reserve,
            self.base_asset_reserve,
            self.peg_multiplier,
        )
    }

    pub fn bid_price(&self, reserve_price: u64) -> ClearingHouseResult<u64> {
        reserve_price
            .cast::<u128>()?
            .safe_mul(BID_ASK_SPREAD_PRECISION_U128.safe_sub(self.short_spread.cast()?)?)?
            .safe_div(BID_ASK_SPREAD_PRECISION_U128)?
            .cast()
    }

    pub fn ask_price(&self, reserve_price: u64) -> ClearingHouseResult<u64> {
        reserve_price
            .cast::<u128>()?
            .safe_mul(BID_ASK_SPREAD_PRECISION_U128.safe_add(self.long_spread.cast()?)?)?
            .safe_div(BID_ASK_SPREAD_PRECISION_U128)?
            .cast::<u64>()
    }

    pub fn bid_ask_price(&self, reserve_price: u64) -> ClearingHouseResult<(u64, u64)> {
        let bid_price = self.bid_price(reserve_price)?;
        let ask_price = self.ask_price(reserve_price)?;
        Ok((bid_price, ask_price))
    }

    pub fn can_lower_k(&self) -> ClearingHouseResult<bool> {
        let can_lower = self.base_asset_amount_with_amm.unsigned_abs() < self.sqrt_k / 4;
        Ok(can_lower)
    }

    pub fn get_oracle_twap(&self, price_oracle: &AccountInfo) -> ClearingHouseResult<Option<i64>> {
        match self.oracle_source {
            OracleSource::Pyth => Ok(Some(self.get_pyth_twap(price_oracle)?)),
            OracleSource::Switchboard => Ok(None),
            OracleSource::QuoteAsset => panic!(),
        }
    }

    pub fn get_pyth_twap(&self, price_oracle: &AccountInfo) -> ClearingHouseResult<i64> {
        let pyth_price_data = price_oracle
            .try_borrow_data()
            .or(Err(ErrorCode::UnableToLoadOracle))?;
        let price_data = pyth_client::cast::<pyth_client::Price>(&pyth_price_data);

        let oracle_twap = price_data.twap.val;

        assert!(oracle_twap > price_data.agg.price / 10);

        let oracle_precision = 10_u128.pow(price_data.expo.unsigned_abs());

        let mut oracle_scale_mult = 1;
        let mut oracle_scale_div = 1;

        if oracle_precision > PRICE_PRECISION {
            oracle_scale_div = oracle_precision.safe_div(PRICE_PRECISION)?;
        } else {
            oracle_scale_mult = PRICE_PRECISION.safe_div(oracle_precision)?;
        }

        oracle_twap
            .cast::<i128>()?
            .safe_mul(oracle_scale_mult.cast()?)?
            .safe_div(oracle_scale_div.cast()?)?
            .cast::<i64>()
    }

    pub fn update_volume_24h(
        &mut self,
        quote_asset_amount: u64,
        position_direction: PositionDirection,
        now: i64,
    ) -> ClearingHouseResult {
        let since_last = max(1, now.safe_sub(self.last_trade_ts)?).cast::<i128>()?;

        amm::update_amm_long_short_intensity(self, now, quote_asset_amount, position_direction)?;

        self.volume_24h = stats::calculate_rolling_sum(
            self.volume_24h,
            quote_asset_amount,
            since_last,
            TWENTY_FOUR_HOUR as i128,
        )?;

        self.last_trade_ts = now;

        Ok(())
    }
}
