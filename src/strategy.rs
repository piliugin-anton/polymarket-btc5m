//! Entry-signal rubric for Up/Down crypto windows.
//!
//! **Settlement (what actually wins):** at round close, **UP** pays off if the final reference price is
//! **higher** than **Price to Beat**; **DOWN** pays off if the final price is **lower**. This module
//! only compares **live spot** vs Price to Beat to suggest which *side* to consider — it does not
//! implement resolution.
//!
//! Switch **base** evaluator with env `STRATEGY=rubric|catch-up` ([`crate::config::SignalStrategy`]);
//! the default rubric compares spot vs PTB on the momentum side. **Catch-up** fades that move when
//! best-ask skew shows the mover outcome richer than the lagging leg (see [`evaluate_catch_up_signal`]).
//!
//! Tunables (`ManualSignalInput`): `strong_gap_mult`, `max_spread_mult`, `min_top_ask_shares`, and
//! `watch_ratio` are loaded from `.env` via [`crate::app::AppState`] (defaults preserve the original
//! rubric). See project README — **Strategy signal tuning**.
//!
//! At evaluation time, tunables are **sanitized** (finite checks + numeric clamps) so callers that
//! bypass [`crate::config::Config`]—tests, JSONL replay, or bad snapshots—cannot collapse the strong
//! gap to a near-zero threshold or widen spread gates with non-finite values. Bounds match the
//! `parse_strategy_*` helpers in [`crate::config`] except `watch_ratio`, which keeps the wider
//! `0.25..=0.90` clamp used here historically.

use crate::config::SignalStrategy;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualSignalLabel {
    NoTrade,
    Watch,
    StrongUp,
    StrongDown,
}

impl ManualSignalLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            ManualSignalLabel::NoTrade => "NO TRADE",
            ManualSignalLabel::Watch => "WATCH",
            ManualSignalLabel::StrongUp => "STRONG UP",
            ManualSignalLabel::StrongDown => "STRONG DOWN",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualSignalSentiment {
    Up,
    Down,
    Neutral,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct ManualSignalBookSide {
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub best_ask_size: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct ManualSignalInput {
    pub spot_price: Option<f64>,
    pub price_to_beat: Option<f64>,
    pub up: ManualSignalBookSide,
    pub down: ManualSignalBookSide,
    pub seconds_to_close: Option<i64>,
    pub window_secs: Option<i64>,
    pub sentiment: ManualSignalSentiment,
    pub activity_notional_60s: f64,
    /// Scales the required spot-vs-target gap for `STRONG` (`1.0` = default; lower → more signals).
    pub strong_gap_mult: f64,
    /// Scales the max bid–ask spread allowed for a usable book (`1.0` = default; higher → more signals, worse execution risk).
    pub max_spread_mult: f64,
    /// Minimum best-ask size (shares) for a usable book (`5.0` = default; lower → more signals, thinner book risk).
    pub min_top_ask_shares: f64,
    /// `WATCH` when `gap >= strong_gap * watch_ratio` (`0.60` = default; lower → more `WATCH`, same `STRONG` threshold).
    pub watch_ratio: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
}

/// Minimum `best_ask` difference (mover minus fade leg) required for [`evaluate_catch_up_signal`].
const CATCHUP_MIN_ASK_EDGE: f64 = 0.02;
/// Clamps match [`crate::config`] `parse_strategy_strong_gap_mult` / `max` / `min_top_ask_shares`.
fn sanitize_strong_gap_mult(v: f64) -> f64 {
    if !v.is_finite() {
        1.0
    } else {
        v.clamp(0.55, 1.15)
    }
}

fn sanitize_max_spread_mult(v: f64) -> f64 {
    if !v.is_finite() {
        1.0
    } else {
        v.clamp(1.0, 1.35)
    }
}

fn sanitize_min_top_ask_shares(v: f64) -> f64 {
    if !v.is_finite() {
        5.0
    } else {
        v.clamp(2.0, 50.0)
    }
}

fn sanitize_watch_ratio(v: f64) -> f64 {
    if !v.is_finite() {
        0.60
    } else {
        v.clamp(0.25, 0.90)
    }
}

fn finite_activity_notional(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

pub fn evaluate_signal(strategy: SignalStrategy, input: &ManualSignalInput) -> ManualSignalLabel {
    match strategy {
        SignalStrategy::Rubric => evaluate_manual_signal(input),
        SignalStrategy::CatchUp => evaluate_catch_up_signal(input),
    }
}

/// Fade the spot-vs-PTB move when the **mover** outcome’s best ask is sufficiently richer than the
/// fade leg’s ask (book “caught up” on the move). Uses the same gap / liquidity / time weighting as
/// the rubric on the **fade** side.
pub fn evaluate_catch_up_signal(input: &ManualSignalInput) -> ManualSignalLabel {
    let Some(spot) = finite_positive(input.spot_price) else {
        return ManualSignalLabel::NoTrade;
    };
    let Some(target) = finite_positive(input.price_to_beat) else {
        return ManualSignalLabel::NoTrade;
    };
    let diff = spot - target;
    if diff == 0.0 {
        return ManualSignalLabel::NoTrade;
    }
    let gap = (diff / target).abs();

    let Some(up_ask) = finite_positive(input.up.best_ask) else {
        return ManualSignalLabel::NoTrade;
    };
    let Some(down_ask) = finite_positive(input.down.best_ask) else {
        return ManualSignalLabel::NoTrade;
    };

    let direction = if diff > 0.0 {
        if up_ask - down_ask + 1e-12 < CATCHUP_MIN_ASK_EDGE {
            return ManualSignalLabel::NoTrade;
        }
        Direction::Down
    } else {
        if down_ask - up_ask + 1e-12 < CATCHUP_MIN_ASK_EDGE {
            return ManualSignalLabel::NoTrade;
        }
        Direction::Up
    };

    evaluate_directional_signal(input, direction, gap)
}

pub fn evaluate_manual_signal(input: &ManualSignalInput) -> ManualSignalLabel {
    let Some(spot) = finite_positive(input.spot_price) else {
        return ManualSignalLabel::NoTrade;
    };
    let Some(target) = finite_positive(input.price_to_beat) else {
        return ManualSignalLabel::NoTrade;
    };
    let diff = spot - target;
    let gap = (diff / target).abs();
    // Align with settlement: UP wins if *close* > Price to Beat, DOWN wins if *close* < Price to Beat.
    // We only observe live `spot`; favor UP when spot is already above target, DOWN when below.
    let direction = if diff > 0.0 {
        Direction::Up
    } else if diff < 0.0 {
        Direction::Down
    } else {
        return ManualSignalLabel::NoTrade;
    };

    evaluate_directional_signal(input, direction, gap)
}

fn evaluate_directional_signal(
    input: &ManualSignalInput,
    direction: Direction,
    gap: f64,
) -> ManualSignalLabel {
    let strong_gap_mult = sanitize_strong_gap_mult(input.strong_gap_mult);
    let max_spread_mult = sanitize_max_spread_mult(input.max_spread_mult);
    let min_top_ask_shares = sanitize_min_top_ask_shares(input.min_top_ask_shares);
    let watch_ratio_clamped = sanitize_watch_ratio(input.watch_ratio);

    let max_spread_raw =
        adaptive_max_spread(input.seconds_to_close, input.window_secs) * max_spread_mult;
    let max_spread = if max_spread_raw.is_finite() {
        max_spread_raw.clamp(0.005, 0.12)
    } else {
        0.02
    };
    let candidate_book = match direction {
        Direction::Up => input.up,
        Direction::Down => input.down,
    };
    if !candidate_book.is_usable(max_spread, min_top_ask_shares) {
        return ManualSignalLabel::NoTrade;
    }

    let activity = finite_activity_notional(input.activity_notional_60s);
    let strong_gap = (required_strong_gap(direction, input, activity) * strong_gap_mult).max(1e-9);
    if gap >= strong_gap {
        return match direction {
            Direction::Up => ManualSignalLabel::StrongUp,
            Direction::Down => ManualSignalLabel::StrongDown,
        };
    }
    let watch_ratio = watch_ratio_clamped;
    if gap >= strong_gap * watch_ratio {
        ManualSignalLabel::Watch
    } else {
        ManualSignalLabel::NoTrade
    }
}

impl ManualSignalBookSide {
    fn is_usable(self, max_spread: f64, min_top_ask_shares: f64) -> bool {
        let Some(bid) = finite_non_negative(self.best_bid) else {
            return false;
        };
        let Some(ask) = finite_non_negative(self.best_ask) else {
            return false;
        };
        let Some(ask_size) = finite_positive(self.best_ask_size) else {
            return false;
        };
        let min_ask = if min_top_ask_shares.is_finite() && min_top_ask_shares > 0.0 {
            min_top_ask_shares
        } else {
            5.0
        };
        (0.01..=0.99).contains(&bid)
            && (0.01..=0.99).contains(&ask)
            && ask > bid
            && ask_size >= min_ask
            && (ask - bid) <= max_spread
    }
}

fn finite_positive(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite() && *x > 0.0)
}

fn finite_non_negative(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite() && *x >= 0.0)
}

fn adaptive_max_spread(seconds_to_close: Option<i64>, window_secs: Option<i64>) -> f64 {
    let Some(secs) = seconds_to_close else {
        return 0.02;
    };
    let Some(window) = window_secs.filter(|w| *w > 0) else {
        return if secs <= 30 { 0.04 } else { 0.02 };
    };
    let elapsed_ratio = 1.0 - (secs.max(0) as f64 / window as f64);
    if secs <= 30 || elapsed_ratio >= 0.90 {
        0.04
    } else if secs <= 90 || elapsed_ratio >= 0.66 {
        0.03
    } else {
        0.02
    }
}

fn required_strong_gap(
    direction: Direction,
    input: &ManualSignalInput,
    activity_notional: f64,
) -> f64 {
    let base = time_weighted_strong_gap(input.seconds_to_close, input.window_secs);

    let sentiment_mult = match (direction, input.sentiment) {
        (Direction::Up, ManualSignalSentiment::Down)
        | (Direction::Down, ManualSignalSentiment::Up) => 1.25,
        (Direction::Up, ManualSignalSentiment::Up)
        | (Direction::Down, ManualSignalSentiment::Down) => 0.90,
        _ => 1.0,
    };
    let activity_mult = if activity_notional >= 1_000.0 {
        0.90
    } else if activity_notional > 0.0 && activity_notional < 100.0 {
        1.15
    } else {
        1.0
    };

    base * sentiment_mult * activity_mult
}

fn time_weighted_strong_gap(seconds_to_close: Option<i64>, window_secs: Option<i64>) -> f64 {
    let Some(secs) = seconds_to_close else {
        return 0.0020;
    };
    let Some(window) = window_secs.filter(|w| *w > 0) else {
        return if secs <= 30 { 0.0008 } else { 0.0020 };
    };

    let elapsed_ratio = 1.0 - (secs.max(0) as f64 / window as f64);
    if window >= 900 {
        if secs <= 60 || elapsed_ratio >= 0.90 {
            0.0008
        } else if elapsed_ratio >= 0.66 {
            0.0018
        } else if elapsed_ratio >= 0.33 {
            0.0035
        } else {
            0.0060
        }
    } else if secs <= 30 || elapsed_ratio >= 0.90 {
        0.0008
    } else if elapsed_ratio >= 0.66 {
        0.0012
    } else if elapsed_ratio >= 0.33 {
        0.0020
    } else {
        0.0045
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::SignalStrategy;

    fn side(best_bid: f64, best_ask: f64, best_ask_size: f64) -> ManualSignalBookSide {
        ManualSignalBookSide {
            best_bid: Some(best_bid),
            best_ask: Some(best_ask),
            best_ask_size: Some(best_ask_size),
        }
    }

    fn base_input() -> ManualSignalInput {
        ManualSignalInput {
            spot_price: Some(100.0),
            price_to_beat: Some(100.0),
            up: side(0.49, 0.50, 25.0),
            down: side(0.50, 0.51, 25.0),
            seconds_to_close: Some(120),
            window_secs: Some(300),
            sentiment: ManualSignalSentiment::Neutral,
            activity_notional_60s: 500.0,
            strong_gap_mult: 1.0,
            max_spread_mult: 1.0,
            min_top_ask_shares: 5.0,
            watch_ratio: 0.60,
        }
    }

    #[test]
    fn strong_gap_mult_below_one_can_promote_to_strong() {
        let mut input = base_input();
        // Early 5m window: base strong gap is high; this spot move is only a WATCH at default mult.
        input.spot_price = Some(100.40);
        input.seconds_to_close = Some(240);
        input.window_secs = Some(300);
        input.up = side(0.49, 0.50, 80.0);
        input.down = side(0.49, 0.50, 80.0);
        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::Watch);

        input.strong_gap_mult = 0.80;
        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn wide_spread_blocks_signal_even_when_spot_gap_is_large() {
        let mut input = base_input();
        input.spot_price = Some(101.0);
        input.up = side(0.30, 0.36, 50.0);
        input.down = side(0.64, 0.65, 50.0);
        input.seconds_to_close = Some(20);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn strong_up_allows_cheap_contrarian_when_spot_gap_and_liquidity_are_extreme() {
        let mut input = base_input();
        input.spot_price = Some(100.80);
        input.up = side(0.34, 0.35, 80.0);
        input.down = side(0.65, 0.66, 80.0);
        input.sentiment = ManualSignalSentiment::Down;

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn mid_round_moderate_edge_can_be_strong_at_any_candidate_price() {
        let mut input = base_input();
        input.spot_price = Some(100.20);
        input.up = side(0.34, 0.35, 80.0);
        input.down = side(0.65, 0.66, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn strong_down_mirrors_strong_up() {
        let mut input = base_input();
        input.spot_price = Some(99.20);
        input.up = side(0.57, 0.58, 70.0);
        input.down = side(0.42, 0.43, 70.0);
        input.sentiment = ManualSignalSentiment::Up;

        assert_eq!(
            evaluate_manual_signal(&input),
            ManualSignalLabel::StrongDown
        );
    }

    #[test]
    fn missing_market_data_blocks_signals() {
        let mut input = base_input();
        input.price_to_beat = None;

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn equal_spot_and_target_is_no_trade_even_with_liquid_books() {
        let input = base_input();

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn price_outside_game_range_blocks_signal() {
        let mut input = base_input();
        input.spot_price = Some(102.0);
        input.up = side(0.001, 0.005, 80.0);
        input.down = side(0.98, 0.99, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn thin_top_ask_liquidity_blocks_signal() {
        let mut input = base_input();
        input.spot_price = Some(101.0);
        input.up = side(0.44, 0.45, 4.99);
        input.down = side(0.54, 0.55, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn early_round_uses_strict_spread_gate() {
        let mut input = base_input();
        input.spot_price = Some(101.0);
        input.seconds_to_close = Some(240);
        input.window_secs = Some(300);
        input.up = side(0.429, 0.45, 80.0);
        input.down = side(0.54, 0.55, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn late_round_allows_wider_spread_when_other_evidence_is_strong() {
        let mut input = base_input();
        input.spot_price = Some(101.0);
        input.seconds_to_close = Some(20);
        input.window_secs = Some(300);
        input.up = side(0.315, 0.35, 80.0);
        input.down = side(0.64, 0.66, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn candidate_price_does_not_change_required_edge() {
        let mut input = base_input();
        input.spot_price = Some(100.20);
        input.up = side(0.74, 0.75, 80.0);
        input.down = side(0.24, 0.25, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);

        input.up = side(0.24, 0.25, 80.0);
        input.down = side(0.74, 0.75, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn late_round_small_edge_can_be_strong_because_close_price_decides() {
        let mut input = base_input();
        input.spot_price = Some(100.09);
        input.seconds_to_close = Some(20);
        input.window_secs = Some(300);
        input.up = side(0.49, 0.50, 80.0);
        input.down = side(0.49, 0.50, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn early_round_same_edge_is_no_trade_due_to_mean_reversion_risk() {
        let mut input = base_input();
        input.spot_price = Some(100.20);
        input.seconds_to_close = Some(240);
        input.window_secs = Some(300);
        input.up = side(0.49, 0.50, 80.0);
        input.down = side(0.49, 0.50, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn fifteen_minute_early_round_requires_more_edge_than_five_minute() {
        let mut input = base_input();
        input.spot_price = Some(100.45);
        input.seconds_to_close = Some(240);
        input.window_secs = Some(300);
        input.up = side(0.49, 0.50, 80.0);
        input.down = side(0.49, 0.50, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);

        input.seconds_to_close = Some(720);
        input.window_secs = Some(900);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::Watch);
    }

    #[test]
    fn only_candidate_side_needs_usable_book() {
        let mut input = base_input();
        input.spot_price = Some(100.20);
        input.up = side(0.49, 0.50, 80.0);
        input.down = side(0.30, 0.36, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn aligned_sentiment_can_lift_borderline_signal_to_strong() {
        let mut input = base_input();
        input.spot_price = Some(100.19);
        input.up = side(0.44, 0.45, 80.0);
        input.down = side(0.54, 0.55, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::Watch);

        input.sentiment = ManualSignalSentiment::Up;

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn opposing_sentiment_can_downgrade_borderline_signal_to_watch() {
        let mut input = base_input();
        input.spot_price = Some(100.21);
        input.up = side(0.44, 0.45, 80.0);
        input.down = side(0.54, 0.55, 80.0);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);

        input.sentiment = ManualSignalSentiment::Down;

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::Watch);
    }

    #[test]
    fn low_activity_requires_more_edge_than_high_activity() {
        let mut input = base_input();
        input.spot_price = Some(100.21);
        input.up = side(0.44, 0.45, 80.0);
        input.down = side(0.54, 0.55, 80.0);
        input.activity_notional_60s = 50.0;

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::Watch);

        input.activity_notional_60s = 1_500.0;

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }

    #[test]
    fn invalid_numbers_block_signal() {
        let mut input = base_input();
        input.spot_price = Some(f64::NAN);

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);

        input = base_input();
        input.up = ManualSignalBookSide {
            best_bid: Some(0.44),
            best_ask: Some(f64::INFINITY),
            best_ask_size: Some(80.0),
        };

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    /// Without sanitization, `strong_gap_mult = NaN` made `strong_gap` collapse to `1e-9` and could
    /// promote tiny gaps to `STRONG`.
    #[test]
    fn nan_strong_gap_mult_does_not_collapse_threshold() {
        let mut input = base_input();
        input.spot_price = Some(100.01);
        input.price_to_beat = Some(100.0);
        input.strong_gap_mult = f64::NAN;

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn catch_up_blocks_when_ask_skew_below_min_edge() {
        let mut input = base_input();
        input.spot_price = Some(101.0);
        input.price_to_beat = Some(100.0);
        input.seconds_to_close = Some(20);
        input.window_secs = Some(300);
        // Skew 0.01 < CATCHUP_MIN_ASK_EDGE
        input.up = side(0.50, 0.51, 80.0);
        input.down = side(0.48, 0.50, 80.0);

        assert_eq!(evaluate_catch_up_signal(&input), ManualSignalLabel::NoTrade);
    }

    #[test]
    fn catch_up_strong_down_when_mover_rich_and_fade_book_usable() {
        let mut input = base_input();
        input.spot_price = Some(101.0);
        input.price_to_beat = Some(100.0);
        input.seconds_to_close = Some(20);
        input.window_secs = Some(300);
        input.up = side(0.64, 0.65, 80.0);
        input.down = side(0.29, 0.30, 80.0);

        assert_eq!(
            evaluate_catch_up_signal(&input),
            ManualSignalLabel::StrongDown
        );
    }

    #[test]
    fn catch_up_strong_up_mirrors_when_spot_below_ptb() {
        let mut input = base_input();
        input.spot_price = Some(99.0);
        input.price_to_beat = Some(100.0);
        input.seconds_to_close = Some(20);
        input.window_secs = Some(300);
        input.up = side(0.29, 0.30, 80.0);
        input.down = side(0.64, 0.65, 80.0);

        assert_eq!(
            evaluate_catch_up_signal(&input),
            ManualSignalLabel::StrongUp
        );
    }

    #[test]
    fn evaluate_signal_dispatches_rubric_vs_catch_up() {
        let mut input = base_input();
        input.spot_price = Some(101.0);
        input.price_to_beat = Some(100.0);
        input.seconds_to_close = Some(20);
        input.window_secs = Some(300);
        input.up = side(0.64, 0.65, 80.0);
        input.down = side(0.29, 0.30, 80.0);

        assert_eq!(
            evaluate_signal(SignalStrategy::Rubric, &input),
            ManualSignalLabel::StrongUp
        );
        assert_eq!(
            evaluate_signal(SignalStrategy::CatchUp, &input),
            ManualSignalLabel::StrongDown
        );
    }

    #[test]
    fn nan_watch_ratio_defaults_like_sensible_tuner() {
        let mut input = base_input();
        input.spot_price = Some(100.20);
        input.watch_ratio = f64::NAN;

        assert_eq!(evaluate_manual_signal(&input), ManualSignalLabel::StrongUp);
    }
}
