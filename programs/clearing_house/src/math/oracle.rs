use crate::error::ClearingHouseResult;
use crate::math::amm;
use crate::state::market::AMM;
use crate::state::oracle::OraclePriceData;
use crate::state::state::OracleGuardRails;

pub fn block_operation(
    amm: &AMM,
    oracle_price_data: &OraclePriceData,
    guard_rails: &OracleGuardRails,
    precomputed_mark_price: Option<u128>,
) -> ClearingHouseResult<bool> {
    let OracleStatus {
        is_valid: oracle_is_valid,
        mark_too_divergent: is_oracle_mark_too_divergent,
        oracle_mark_spread_pct: _,
        ..
    } = get_oracle_status(amm, oracle_price_data, guard_rails, precomputed_mark_price)?;

    let block = !oracle_is_valid || is_oracle_mark_too_divergent;
    Ok(block)
}

#[derive(Default, Clone, Copy, Debug)]
pub struct OracleStatus {
    pub price_data: OraclePriceData,
    pub oracle_mark_spread_pct: i128,
    pub is_valid: bool,
    pub mark_too_divergent: bool,
}

pub fn get_oracle_status<'a>(
    amm: &AMM,
    oracle_price_data: &'a OraclePriceData,
    guard_rails: &OracleGuardRails,
    precomputed_mark_price: Option<u128>,
) -> ClearingHouseResult<OracleStatus> {
    let oracle_is_valid = amm::is_oracle_valid(amm, oracle_price_data, &guard_rails.validity)?;
    let oracle_mark_spread_pct =
        amm::calculate_oracle_mark_spread_pct(amm, oracle_price_data, precomputed_mark_price)?;
    let is_oracle_mark_too_divergent =
        amm::is_oracle_mark_too_divergent(oracle_mark_spread_pct, &guard_rails.price_divergence)?;

    Ok(OracleStatus {
        price_data: *oracle_price_data,
        oracle_mark_spread_pct,
        is_valid: oracle_is_valid,
        mark_too_divergent: is_oracle_mark_too_divergent,
    })
}
