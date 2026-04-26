//! Pure helpers for market-buy take-profit **consolidation** and user-WS **merge** of duplicate
//! resting SELL legs (unit-tested). Async CLOB I/O stays in `main.rs`.

use crate::app::Outcome;
use crate::trading::{clob_asset_ids_match, parse_clob_side_str, ClobOpenOrder, Side};

/// Unfilled size (`original_size - size_matched`), clamped to `>= 0`, `0` if parse fails.
pub fn clob_order_remaining_size(o: &ClobOpenOrder) -> f64 {
    let orig = o.original_size.parse::<f64>().unwrap_or(f64::NAN);
    let matched = o.size_matched.parse::<f64>().unwrap_or(0.0);
    if orig.is_finite() {
        (orig - matched).max(0.0)
    } else {
        0.0
    }
}

/// GTD take-profit sell size after a market BUY consolidate.
///
/// - `had_resting_sells`: sum old TP escrow (`sell_rem_pre`) + this fill (`buy_ack_qty`).
/// - no existing TP: use total position (`position_shares`), with `buy_ack_qty` as floor for stale state.
pub fn consolidate_tp_want_shares(
    position_shares: f64,
    sell_rem_pre: f64,
    buy_ack_qty: f64,
    had_resting_sells: bool,
) -> f64 {
    if had_resting_sells {
        let old = if sell_rem_pre.is_finite() { sell_rem_pre.max(0.0) } else { 0.0 };
        let new = if buy_ack_qty.is_finite() { buy_ack_qty.max(0.0) } else { 0.0 };
        old + new
    } else {
        let mut want = if position_shares.is_finite() {
            position_shares.max(0.0)
        } else {
            0.0
        };
        if buy_ack_qty.is_finite() && buy_ack_qty > 1e-9 {
            want = want.max(buy_ack_qty);
        }
        want
    }
}

/// Outcomes (UP/DOWN) with **≥ 2** resting **SELL** rows on that token — user-WS merge trigger.
pub fn outcomes_with_duplicate_resting_sells(
    rows: &[ClobOpenOrder],
    up_token_id: &str,
    down_token_id: &str,
) -> Vec<Outcome> {
    let mut up_n = 0usize;
    let mut down_n = 0usize;
    for o in rows {
        if parse_clob_side_str(&o.side) != Some(Side::Sell) {
            continue;
        }
        if clob_asset_ids_match(&o.asset_id, up_token_id) {
            up_n += 1;
        } else if clob_asset_ids_match(&o.asset_id, down_token_id) {
            down_n += 1;
        }
    }
    let mut out = Vec::new();
    if up_n >= 2 {
        out.push(Outcome::Up);
    }
    if down_n >= 2 {
        out.push(Outcome::Down);
    }
    out
}

/// If there are at least two resting SELL rows in `sells`, returns **sum** of their remaining sizes;
/// otherwise `None` (no merge).
pub fn merge_duplicate_sells_total_if_eligible(sells: &[&ClobOpenOrder]) -> Option<f64> {
    if sells.len() < 2 {
        return None;
    }
    let sum: f64 = sells.iter().map(|o| clob_order_remaining_size(o)).sum();
    Some(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sell_row(id: &str, asset: &str, orig: &str, matched: &str) -> ClobOpenOrder {
        ClobOpenOrder {
            id: id.to_string(),
            asset_id: asset.to_string(),
            side: "SELL".to_string(),
            price: "0.55".to_string(),
            original_size: orig.to_string(),
            size_matched: matched.to_string(),
        }
    }

    fn buy_row(asset: &str) -> ClobOpenOrder {
        ClobOpenOrder {
            id: "b1".into(),
            asset_id: asset.into(),
            side: "BUY".into(),
            price: "0.5".into(),
            original_size: "10".into(),
            size_matched: "0".into(),
        }
    }

    #[test]
    fn remaining_size_normal_and_fully_matched() {
        let o = sell_row("a", "111", "10", "3");
        assert!((clob_order_remaining_size(&o) - 7.0).abs() < 1e-9);
        let o2 = sell_row("b", "111", "10", "10");
        assert!(clob_order_remaining_size(&o2).abs() < 1e-9);
    }

    #[test]
    fn remaining_size_bad_original_yields_zero() {
        let mut o = sell_row("x", "111", "10", "0");
        o.original_size = "nan".into();
        assert!(clob_order_remaining_size(&o).abs() < 1e-9);
    }

    #[test]
    fn consolidate_sums_escrow_and_ack_when_had_resting_sells() {
        // old TP escrow + new fill = sum regardless of stale position_shares
        assert!(
            (consolidate_tp_want_shares(15.0, 10.0, 5.0, true) - 15.0).abs() < 1e-9,
            "10 escrow + 5 fill = 15"
        );
        assert!(
            (consolidate_tp_want_shares(2.0, 10.0, 5.0, true) - 15.0).abs() < 1e-9,
            "stale position 2 ignored; 10 + 5 = 15"
        );
        assert!(
            (consolidate_tp_want_shares(99.0, 10.0, 5.0, true) - 15.0).abs() < 1e-9,
            "position_shares not used when had_resting_sells"
        );
    }

    #[test]
    fn consolidate_no_resting_uses_position_with_ack_floor() {
        assert!(
            (consolidate_tp_want_shares(3.0, 0.0, 10.0, false) - 10.0).abs() < 1e-9,
            "buy ack floor without resting sells"
        );
        assert!(
            (consolidate_tp_want_shares(12.0, 100.0, 5.0, false) - 12.0).abs() < 1e-9,
            "sell_rem ignored when no resting sells; position 12 > ack 5"
        );
    }

    #[test]
    fn consolidate_non_finite_position_still_picks_ack() {
        let v = consolidate_tp_want_shares(f64::NAN, 0.0, 6.0, false);
        assert!((v - 6.0).abs() < 1e-9);
    }

    #[test]
    fn consolidate_non_finite_sell_rem_uses_only_ack() {
        let v = consolidate_tp_want_shares(0.0, f64::NAN, 6.0, true);
        assert!((v - 6.0).abs() < 1e-9, "NaN escrow falls back to buy_ack");
    }

    #[test]
    fn outcomes_empty_and_single_sell_each_side() {
        let up = "111";
        let down = "222";
        assert!(outcomes_with_duplicate_resting_sells(&[], up, down).is_empty());
        let one = vec![sell_row("1", up, "5", "0")];
        assert!(outcomes_with_duplicate_resting_sells(&one, up, down).is_empty());
    }

    #[test]
    fn outcomes_two_sells_up_triggers_merge() {
        let up = "111";
        let down = "222";
        let rows = vec![
            sell_row("1", up, "5", "0"),
            sell_row("2", up, "7", "0"),
        ];
        assert_eq!(
            outcomes_with_duplicate_resting_sells(&rows, up, down),
            vec![Outcome::Up]
        );
    }

    #[test]
    fn outcomes_two_down_one_up() {
        let up = "111";
        let down = "222";
        let rows = vec![
            sell_row("1", down, "5", "0"),
            sell_row("2", down, "5", "0"),
            sell_row("3", up, "9", "0"),
        ];
        assert_eq!(
            outcomes_with_duplicate_resting_sells(&rows, up, down),
            vec![Outcome::Down]
        );
    }

    #[test]
    fn outcomes_buys_do_not_count_toward_sell_merge() {
        let up = "111";
        let rows = vec![buy_row(up), buy_row(up)];
        assert!(outcomes_with_duplicate_resting_sells(&rows, up, "222").is_empty());
    }

    #[test]
    fn outcomes_both_sides_when_each_has_two_sells() {
        let up = "111";
        let down = "222";
        let rows = vec![
            sell_row("1", up, "5", "0"),
            sell_row("2", up, "5", "0"),
            sell_row("3", down, "3", "0"),
            sell_row("4", down, "3", "0"),
        ];
        let got = outcomes_with_duplicate_resting_sells(&rows, up, down);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&Outcome::Up) && got.contains(&Outcome::Down));
    }

    #[test]
    fn merge_total_none_for_zero_or_one_sell() {
        let a = sell_row("1", "111", "5", "0");
        assert!(merge_duplicate_sells_total_if_eligible(&[]).is_none());
        assert!(merge_duplicate_sells_total_if_eligible(&[&a]).is_none());
    }

    #[test]
    fn merge_total_sums_two_sells() {
        let a = sell_row("1", "111", "5", "1");
        let b = sell_row("2", "111", "10", "0");
        let t = merge_duplicate_sells_total_if_eligible(&[&a, &b]).unwrap();
        assert!((t - (4.0 + 10.0)).abs() < 1e-9, "t={t}");
    }
}
