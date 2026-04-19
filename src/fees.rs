//! Polymarket CLOB **taker** fee helpers (**Crypto** category).
//!
//! Formula and rounding: <https://docs.polymarket.com/trading/fees>
//! `fee = C × feeRate × p × (1 - p)` in USDC, rounded to 5 decimal places.

/// Taker fee rate for **Crypto** markets (`feeRate` in Polymarket docs).
pub const POLYMARKET_CRYPTO_TAKER_FEE_RATE: f64 = 0.072;

#[inline]
fn round_fee_usdc(raw: f64) -> f64 {
    (raw * 100_000.0).round() / 100_000.0
}

/// Taker fee in USDC for `shares` at outcome price `p` (Polymarket probability price).
#[inline]
pub fn polymarket_crypto_taker_fee_usdc(shares: f64, price: f64) -> f64 {
    if shares <= 0.0 || !shares.is_finite() || !price.is_finite() {
        return 0.0;
    }
    let raw = shares * POLYMARKET_CRYPTO_TAKER_FEE_RATE * price * (1.0 - price);
    round_fee_usdc(raw)
}

/// Clamp outcome price to the CLOB’s usual tradable band.
#[inline]
pub(crate) fn clamp_prob_fee(p: f64) -> f64 {
    p.clamp(0.01, 0.99)
}

/// Limit price for a GTD take-profit **sell** after a **taker** market buy at `entry_px`.
///
/// Interprets `take_profit_bps` like the legacy formula (`entry * (1 + bps/10_000)`) but solves for a
/// limit price `tp` such that **round-trip taker fees** still leave about that **gross** edge vs
/// `entry_px` (see Polymarket fee note: makers pay no fee — if the TP fills as maker, realized profit
/// is typically better than this model).
pub fn take_profit_limit_price_crypto_after_fees(entry_px: f64, take_profit_bps: u32) -> f64 {
    let e = clamp_prob_fee(entry_px);
    let r = take_profit_bps as f64 / 10_000.0;
    const FR: f64 = POLYMARKET_CRYPTO_TAKER_FEE_RATE;

    // Cost basis per share after buy-side taker fee: `e` + fee(1 share @ e) / 1
    let c0 = e + FR * e * (1.0 - e);
    let target_net_per_share = c0 + r * e;

    // Net proceeds per share at exit if SELL is taker: `tp - FR*tp*(1-tp)`
    // => FR*tp² + (1-FR)*tp - target = 0
    let disc = (1.0 - FR).mul_add(1.0 - FR, 4.0 * FR * target_net_per_share);
    if disc <= 0.0 || FR.abs() < f64::EPSILON {
        return clamp_prob_fee(e * (1.0 + r));
    }
    let tp = (-(1.0 - FR) + disc.sqrt()) / (2.0 * FR);
    clamp_prob_fee(tp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_table_crypto_100_shares_at_50c() {
        let f = polymarket_crypto_taker_fee_usdc(100.0, 0.50);
        assert!((f - 1.8).abs() < 1e-4);
    }

    #[test]
    fn take_profit_above_legacy_when_fees_positive() {
        let e = 0.50;
        let bps = 100u32;
        let legacy = e * (1.0 + bps as f64 / 10_000.0);
        let adj = take_profit_limit_price_crypto_after_fees(e, bps);
        assert!(adj > legacy, "adj={adj} legacy={legacy}");
    }

    #[test]
    fn take_profit_zero_bps_break_even_round_trip_taker() {
        let e = 0.50;
        let tp = take_profit_limit_price_crypto_after_fees(e, 0);
        let net = tp - POLYMARKET_CRYPTO_TAKER_FEE_RATE * tp * (1.0 - tp);
        let c0 = e + POLYMARKET_CRYPTO_TAKER_FEE_RATE * e * (1.0 - e);
        assert!((net - c0).abs() < 1e-6, "net={net} c0={c0}");
    }
}
