//! Offline replay of strategy labels ([`crate::strategy::evaluate_signal`]) on JSONL `snap` rows, plus an
//! optional **STRONG-only** autotrading-style simulation (time / max entry / max positions / GTD
//! TTL, stop-loss, trailing min-profit exit) using the same env defaults as [`crate::config::Config`]
//! when CLI flags are omitted.
//!
//! **Performance:** after ingest, [`sort_round_snaps_by_ts`] orders snaps once per round so
//! [`aggregate_signal_eval_on_rounds`] and `signal-eval-tune` do not clone or re-sort per grid
//! combination. Bps grind axes share one `Arc<[u32]>` when all three bps grids are omitted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::app::{
    clamp_prob, trailing_exit_sell_meets_min_gross_profit_bps, Outcome, MIN_LIMIT_ORDER_SHARES,
};
use crate::config::{
    parse_autotrading_buy_early_ptb_gap_bps, parse_autotrading_buy_last_secs,
    parse_autotrading_max_entry_price, parse_autotrading_max_positions,
    parse_autotrading_order_expires_after_secs, parse_stop_loss_bps,
    parse_trailing_exit_min_profit_bps, SignalStrategy,
};
use crate::market_profile::MarketProfile;
use crate::round_log::{
    self, u8_to_label, u8_to_sentiment, RoundLogStrategyTunables,
};
use crate::stop_loss::{stop_loss_sell_limit_price, stop_loss_triggered};
use crate::strategy::{
    evaluate_signal, ManualSignalBookSide, ManualSignalInput, ManualSignalLabel,
};

/// Resolve `STRATEGY` for offline tools: explicit `--strategy` wins, else `STRATEGY` env, else rubric.
fn signal_strategy_cli_or_env(cli: Option<&str>) -> Result<SignalStrategy> {
    if let Some(raw) = cli {
        let t = raw.trim();
        if t.is_empty() {
            bail!("--strategy requires rubric or catch-up");
        }
        SignalStrategy::parse_env(Some(t))
    } else {
        SignalStrategy::parse_env(std::env::var("STRATEGY").ok().as_deref())
    }
}

fn strategy_label(s: SignalStrategy) -> &'static str {
    match s {
        SignalStrategy::Rubric => "rubric",
        SignalStrategy::CatchUp => "catch-up",
    }
}
const SIM_DEFAULT_SIZE_USDC: f64 = 5.0;
/// Default sell slippage bps for simulated trailing-floor checks (matches `MARKET_SELL_SLIPPAGE_BPS`).
const SIM_DEFAULT_SELL_SLIPPAGE_BPS: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalMode {
    StrongOnly,
    WatchAsHint,
}

impl EvalMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "strong-only" => Some(Self::StrongOnly),
            "watch-as-hint" => Some(Self::WatchAsHint),
            _ => None,
        }
    }
}

/// One condition (round) aggregated from JSONL `open` / `snap` / `close` lines.
#[derive(Default)]
pub(crate) struct RoundAccum {
    pub ws: Option<i64>,
    pub we: Option<i64>,
    pub tunables: Option<RoundLogStrategyTunables>,
    pub snaps: Vec<SnapParsed>,
    pub win: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SnapParsed {
    _ts: i64,
    spot: Option<f64>,
    ptb: Option<f64>,
    sig: u8,
    sent: u8,
    ubu: Option<f64>,
    uba: Option<f64>,
    dbu: Option<f64>,
    dba: Option<f64>,
    ubas: Option<f64>,
    dbas: Option<f64>,
    secs: Option<i64>,
    act: f64,
}

#[derive(Clone, Serialize)]
struct EvalSummary {
    rounds_total: usize,
    rounds_with_win: usize,
    snaps_total: u64,
    snaps_usable_book: u64,
    mode: String,
    strong_calls: u64,
    strong_correct: u64,
    watch_calls: u64,
    watch_correct: u64,
    replay_mismatch_live_sig: u64,
    sim_trading: SimTradingSummary,
    sim_trading_params: SimTradingParams,
}

/// Sorted paths to `*.jsonl` round logs under `dir`, optionally filtered by calendar `--day`
/// (stem equals `YYYY-MM-DD` or starts with `YYYY-MM-DD-`).
pub fn list_jsonl_round_paths(dir: &Path, day: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for ent in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let ent = ent?;
        let p = ent.path();
        if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
            if let Some(d) = day {
                if p.file_stem()
                    .map(|s| jsonl_stem_matches_day(&s.to_string_lossy(), d))
                    .unwrap_or(false)
                {
                    out.push(p);
                }
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn jsonl_stem_matches_day(stem: &str, day: &str) -> bool {
    stem == day
        || stem
            .strip_prefix(day)
            .is_some_and(|rest| rest.starts_with('-'))
}

fn ingest_jsonl(path: &std::path::Path, rounds: &mut HashMap<String, RoundAccum>) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    for (lineno, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: invalid JSON", path.display(), lineno + 1))?;
        let Some(t) = v.get("t").and_then(|x| x.as_str()) else {
            continue;
        };
        let ver = v.get("v").and_then(|x| x.as_u64()).unwrap_or(0);
        if ver != 1 {
            continue;
        }
        match t {
            "open" => {
                let Some(cid) = v.get("cid").and_then(|x| x.as_str()) else {
                    continue;
                };
                let acc = rounds.entry(cid.to_string()).or_default();
                acc.ws = v.get("ws").and_then(|x| x.as_i64());
                acc.we = v.get("we").and_then(|x| x.as_i64());
                acc.tunables = Some(RoundLogStrategyTunables {
                    strong_gap_mult: v
                        .get("strong_gap_mult")
                        .and_then(|x| x.as_f64())
                        .unwrap_or(1.0),
                    max_spread_mult: v
                        .get("max_spread_mult")
                        .and_then(|x| x.as_f64())
                        .unwrap_or(1.0),
                    min_top_ask_shares: v
                        .get("min_top_ask_shares")
                        .and_then(|x| x.as_f64())
                        .unwrap_or(5.0),
                    watch_ratio: v.get("watch_ratio").and_then(|x| x.as_f64()).unwrap_or(0.6),
                });
            }
            "snap" => {
                let Some(cid) = v.get("cid").and_then(|x| x.as_str()) else {
                    continue;
                };
                let acc = rounds.entry(cid.to_string()).or_default();
                acc.snaps.push(SnapParsed {
                    _ts: v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0),
                    spot: v.get("spot").and_then(|x| x.as_f64()),
                    ptb: v.get("ptb").and_then(|x| x.as_f64()),
                    sig: v.get("sig").and_then(|x| x.as_u64()).unwrap_or(255) as u8,
                    sent: v.get("sent").and_then(|x| x.as_u64()).unwrap_or(255) as u8,
                    ubu: v.get("ubu").and_then(|x| x.as_f64()),
                    uba: v.get("uba").and_then(|x| x.as_f64()),
                    dbu: v.get("dbu").and_then(|x| x.as_f64()),
                    dba: v.get("dba").and_then(|x| x.as_f64()),
                    ubas: v.get("ubas").and_then(|x| x.as_f64()),
                    dbas: v.get("dbas").and_then(|x| x.as_f64()),
                    secs: v.get("secs").and_then(|x| x.as_i64()),
                    act: v.get("act").and_then(|x| x.as_f64()).unwrap_or(0.0),
                });
            }
            "close" => {
                let Some(cid) = v.get("cid").and_then(|x| x.as_str()) else {
                    continue;
                };
                let acc = rounds.entry(cid.to_string()).or_default();
                if let Some(w) = v.get("win").and_then(|x| x.as_str()) {
                    acc.win = Some(w.to_string());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Sort snap rows by timestamp once; replay and simulation require chronological order.
fn sort_round_snaps_by_ts(rounds: &mut HashMap<String, RoundAccum>) {
    for acc in rounds.values_mut() {
        acc.snaps.sort_unstable_by_key(|s| s._ts);
    }
}

fn load_rounds_from_paths(paths: &[PathBuf]) -> Result<HashMap<String, RoundAccum>> {
    let mut rounds: HashMap<String, RoundAccum> = HashMap::new();
    for p in paths {
        ingest_jsonl(p, &mut rounds)?;
    }
    sort_round_snaps_by_ts(&mut rounds);
    Ok(rounds)
}

/// Load and merge JSONL round logs from `dir`, optionally filtered by `--day` stem (same rules as
/// `signal-eval`). Snap rows are sorted by timestamp per round.
pub fn load_offline_rounds(dir: &Path, day: Option<&str>) -> Result<HashMap<String, RoundAccum>> {
    let paths = list_jsonl_round_paths(dir, day)?;
    load_rounds_from_paths(&paths)
}

/// Like [`load_offline_rounds`], but optionally restricts to one market profile and errors when
/// `--day` matches multiple logs without `--profile` (avoids mixing unrelated markets in MCMC).
pub fn load_offline_rounds_filtered(
    dir: &Path,
    day: Option<&str>,
    profile: Option<&str>,
) -> Result<(HashMap<String, RoundAccum>, Vec<PathBuf>)> {
    let mut paths = list_jsonl_round_paths(dir, day)?;
    if let Some(p) = profile {
        let mp = MarketProfile::parse_cli_token(p).map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let (sa, stf) = round_log::round_log_path_suffix_for_profile(&mp);
        let suffix = format!("{sa}-{stf}");
        paths.retain(|path| {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(d) = day {
                stem == round_log::expected_round_log_stem(d, &mp)
            } else {
                stem == suffix || stem.ends_with(&format!("-{suffix}"))
            }
        });
        if paths.is_empty() {
            bail!(
                "no JSONL round logs match --profile {p:?} under {}",
                dir.display()
            );
        }
    } else if day.is_some() && paths.len() > 1 {
        bail!(
            "multiple round logs for day {}; pass --profile (e.g. sol-5m, btc-5m)",
            day.expect("day is_some")
        );
    }
    let rounds = load_rounds_from_paths(&paths)?;
    Ok((rounds, paths))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplayStrategyMetrics {
    pub rounds_with_win: usize,
    pub snaps_total: u64,
    pub snaps_usable_book: u64,
    pub strong_calls: u64,
    pub strong_correct: u64,
    pub watch_calls: u64,
    pub watch_correct: u64,
    pub replay_mismatch_live_sig: u64,
}

/// Replay-only counts (no autotrading simulation). Requires [`sort_round_snaps_by_ts`].
pub fn replay_strategy_metrics(
    rounds: &HashMap<String, RoundAccum>,
    mode: EvalMode,
    strategy: SignalStrategy,
    overrides: &TunableOverrides,
) -> ReplayStrategyMetrics {
    let mut out = ReplayStrategyMetrics::default();
    for (_cid, acc) in rounds {
        let Some(win) = acc.win.as_deref() else {
            continue;
        };
        if win != "up" && win != "down" {
            continue;
        }
        out.rounds_with_win += 1;
        let Some(tun) = acc.tunables.as_ref() else {
            continue;
        };
        let ws = acc.ws.unwrap_or(0);
        let we = acc.we.unwrap_or(ws + 1).max(ws + 1);

        for s in acc.snaps.as_slice() {
            out.snaps_total += 1;
            let Some(input) = snap_to_input(s, tun, ws, we, overrides) else {
                continue;
            };
            out.snaps_usable_book += 1;
            let replay = evaluate_signal(strategy, &input);
            if let Some(live) = u8_to_label(s.sig) {
                if live != replay {
                    out.replay_mismatch_live_sig += 1;
                }
            }

            if let Some(pred) = predicted_side_strong(replay) {
                out.strong_calls += 1;
                if pred == win {
                    out.strong_correct += 1;
                }
            }

            if mode == EvalMode::WatchAsHint && matches!(replay, ManualSignalLabel::Watch) {
                out.watch_calls += 1;
                if let Some(pred) = predicted_side_watch_hint(strategy, replay, s.spot, s.ptb) {
                    if pred == win {
                        out.watch_correct += 1;
                    }
                }
            }
        }
    }
    out
}

fn snap_to_input(
    s: &SnapParsed,
    tunables: &RoundLogStrategyTunables,
    ws: i64,
    we: i64,
    overrides: &TunableOverrides,
) -> Option<ManualSignalInput> {
    let sent = u8_to_sentiment(s.sent)?;
    if s.ubu.is_none()
        || s.uba.is_none()
        || s.dbu.is_none()
        || s.dba.is_none()
        || s.ubas.is_none()
        || s.dbas.is_none()
    {
        return None;
    }
    Some(ManualSignalInput {
        spot_price: s.spot,
        price_to_beat: s.ptb,
        up: ManualSignalBookSide {
            best_bid: s.ubu,
            best_ask: s.uba,
            best_ask_size: s.ubas,
        },
        down: ManualSignalBookSide {
            best_bid: s.dbu,
            best_ask: s.dba,
            best_ask_size: s.dbas,
        },
        seconds_to_close: s.secs,
        window_secs: Some((we - ws).max(1)),
        sentiment: sent,
        activity_notional_60s: s.act,
        strong_gap_mult: overrides
            .strong_gap_mult
            .unwrap_or(tunables.strong_gap_mult),
        max_spread_mult: overrides
            .max_spread_mult
            .unwrap_or(tunables.max_spread_mult),
        min_top_ask_shares: overrides
            .min_top_ask_shares
            .unwrap_or(tunables.min_top_ask_shares),
        watch_ratio: overrides.watch_ratio.unwrap_or(tunables.watch_ratio),
    })
}

#[derive(Default, Clone)]
pub(crate) struct TunableOverrides {
    pub(crate) strong_gap_mult: Option<f64>,
    pub(crate) max_spread_mult: Option<f64>,
    pub(crate) min_top_ask_shares: Option<f64>,
    pub(crate) watch_ratio: Option<f64>,
}

/// Strategy overrides with every field set to `t` (MCMC / global replay over logs).
pub(crate) fn tunable_overrides_global(t: &RoundLogStrategyTunables) -> TunableOverrides {
    TunableOverrides {
        strong_gap_mult: Some(t.strong_gap_mult),
        max_spread_mult: Some(t.max_spread_mult),
        min_top_ask_shares: Some(t.min_top_ask_shares),
        watch_ratio: Some(t.watch_ratio),
    }
}

/// Resolved autotrading / exit simulation parameters (defaults match [`crate::config::Config`]).
#[derive(Debug, Clone, Serialize)]
struct SimTradingParams {
    autotrading_buy_last_secs: Option<u64>,
    autotrading_buy_early_ptb_gap_bps: u32,
    autotrading_order_expires_after_secs: Option<u64>,
    autotrading_max_entry_price: Option<f64>,
    autotrading_max_positions: usize,
    trailing_exit_min_profit_bps: u32,
    stop_loss_bps: u32,
}

impl SimTradingParams {
    fn from_cli_optional(
        buy_last_secs: Option<&str>,
        early_ptb_gap_bps: Option<&str>,
        order_expires_after: Option<&str>,
        max_entry_price: Option<&str>,
        max_positions: Option<&str>,
        trailing_exit_min_profit_bps: Option<&str>,
        stop_loss_bps: Option<&str>,
    ) -> Self {
        Self {
            autotrading_buy_last_secs: parse_autotrading_buy_last_secs(buy_last_secs),
            autotrading_buy_early_ptb_gap_bps: early_ptb_gap_bps
                .map(|s| parse_autotrading_buy_early_ptb_gap_bps(Some(s)))
                .unwrap_or_else(|| parse_autotrading_buy_early_ptb_gap_bps(None)),
            autotrading_order_expires_after_secs: order_expires_after
                .map(|s| parse_autotrading_order_expires_after_secs(Some(s)))
                .unwrap_or_else(|| parse_autotrading_order_expires_after_secs(None)),
            autotrading_max_entry_price: max_entry_price
                .map(|s| parse_autotrading_max_entry_price(Some(s)))
                .unwrap_or_else(|| parse_autotrading_max_entry_price(None)),
            autotrading_max_positions: max_positions
                .map(|s| parse_autotrading_max_positions(Some(s)))
                .unwrap_or_else(|| parse_autotrading_max_positions(None)),
            trailing_exit_min_profit_bps: trailing_exit_min_profit_bps
                .map(|s| parse_trailing_exit_min_profit_bps(Some(s)))
                .unwrap_or_else(|| parse_trailing_exit_min_profit_bps(None)),
            stop_loss_bps: stop_loss_bps
                .map(|s| parse_stop_loss_bps(Some(s)))
                .unwrap_or_else(|| parse_stop_loss_bps(None)),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
struct SimTradingSummary {
    strong_entry_signals: u64,
    entries_filled: u64,
    blocked_buy_last_secs: u64,
    blocked_max_entry_price: u64,
    blocked_max_positions: u64,
    blocked_min_shares: u64,
    pending_buy_expired: u64,
    unfilled_pending_at_round_close: u64,
    exit_stop_loss: u64,
    exit_trailing_min_profit: u64,
    exit_settlement_win: u64,
    exit_settlement_lose: u64,
    realized_pnl_usdc: f64,
}

impl SimTradingSummary {
    fn merge(&mut self, o: &SimTradingSummary) {
        self.strong_entry_signals += o.strong_entry_signals;
        self.entries_filled += o.entries_filled;
        self.blocked_buy_last_secs += o.blocked_buy_last_secs;
        self.blocked_max_entry_price += o.blocked_max_entry_price;
        self.blocked_max_positions += o.blocked_max_positions;
        self.blocked_min_shares += o.blocked_min_shares;
        self.pending_buy_expired += o.pending_buy_expired;
        self.unfilled_pending_at_round_close += o.unfilled_pending_at_round_close;
        self.exit_stop_loss += o.exit_stop_loss;
        self.exit_trailing_min_profit += o.exit_trailing_min_profit;
        self.exit_settlement_win += o.exit_settlement_win;
        self.exit_settlement_lose += o.exit_settlement_lose;
        self.realized_pnl_usdc += o.realized_pnl_usdc;
    }
}

#[derive(Debug, Clone)]
struct PendingBuy {
    limit: f64,
    expires_ts: i64,
}

#[derive(Debug, Clone)]
struct SimOpenLeg {
    outcome: Outcome,
    entry: f64,
    shares: f64,
}

#[derive(Debug, Default)]
struct SimRoundState {
    pending: HashMap<Outcome, PendingBuy>,
    open: Vec<SimOpenLeg>,
    round: SimTradingSummary,
}

fn sim_autotrading_time_window_allows(secs_to_close: Option<i64>, buy_last_secs: Option<u64>) -> bool {
    match buy_last_secs {
        None => true,
        Some(w) => secs_to_close.is_some_and(|s| s > 0 && s <= w as i64),
    }
}

fn sim_autotrading_blocked_only_too_early(
    secs_to_close: Option<i64>,
    buy_last_secs: Option<u64>,
) -> bool {
    match buy_last_secs {
        None => false,
        Some(w) => secs_to_close.is_some_and(|s| s > 0 && s > w as i64),
    }
}

fn sim_autotrading_early_ptb_gap_bypasses(
    spot: Option<f64>,
    ptb: Option<f64>,
    threshold_bps: u32,
) -> bool {
    if threshold_bps == 0 {
        return false;
    }
    let Some(spot) = spot.filter(|p| p.is_finite()) else {
        return false;
    };
    let Some(ptb) = ptb.filter(|p| p.is_finite() && *p > 0.0) else {
        return false;
    };
    let gap_bps = ((spot - ptb).abs() / ptb) * 10_000.0;
    gap_bps + 1e-12 >= threshold_bps as f64
}

fn sim_buy_time_gate_ok(
    secs_to_close: Option<i64>,
    buy_last_secs: Option<u64>,
    early_ptb_gap_bps: u32,
    spot: Option<f64>,
    ptb: Option<f64>,
) -> bool {
    let time_ok = sim_autotrading_time_window_allows(secs_to_close, buy_last_secs);
    let early_bypass = sim_autotrading_blocked_only_too_early(secs_to_close, buy_last_secs)
        && sim_autotrading_early_ptb_gap_bypasses(spot, ptb, early_ptb_gap_bps);
    time_ok || early_bypass
}

fn label_to_autotrading_outcome(label: ManualSignalLabel) -> Option<Outcome> {
    match label {
        ManualSignalLabel::StrongUp => Some(Outcome::Up),
        ManualSignalLabel::StrongDown => Some(Outcome::Down),
        ManualSignalLabel::NoTrade | ManualSignalLabel::Watch => None,
    }
}

fn snap_best_ask(s: &SnapParsed, outcome: Outcome) -> Option<f64> {
    match outcome {
        Outcome::Up => s.uba,
        Outcome::Down => s.dba,
    }
}

fn snap_best_bid(s: &SnapParsed, outcome: Outcome) -> Option<f64> {
    match outcome {
        Outcome::Up => s.ubu,
        Outcome::Down => s.dbu,
    }
}

fn sim_reserved_count(pending: &HashMap<Outcome, PendingBuy>, open: &[SimOpenLeg]) -> usize {
    let mut t = HashSet::new();
    for o in pending.keys() {
        t.insert(*o);
    }
    for leg in open {
        t.insert(leg.outcome);
    }
    t.len()
}

fn sim_pending_expires_ts(placed_ts: i64, order_expires_after: Option<u64>, window_end_ts: i64) -> i64 {
    match order_expires_after {
        Some(d) => placed_ts.saturating_add(d as i64).min(window_end_ts),
        None => window_end_ts,
    }
}

fn sim_try_fill_pending(state: &mut SimRoundState, snap: &SnapParsed, _sim: &SimTradingParams) {
    let mut expired = Vec::new();
    let mut filled = Vec::new();
    for (outcome, p) in &state.pending {
        if snap._ts > p.expires_ts {
            expired.push(*outcome);
            continue;
        }
        let Some(ask) = snap_best_ask(snap, *outcome).filter(|a| a.is_finite() && *a > 0.0) else {
            continue;
        };
        if ask <= p.limit + 1e-12 {
            filled.push(*outcome);
        }
    }
    for o in expired {
        state.pending.remove(&o);
        state.round.pending_buy_expired += 1;
    }
    for o in filled {
        let Some(p) = state.pending.get(&o) else {
            continue;
        };
        // Do not `remove` until we commit the fill — otherwise a failed check would drop the order
        // from `pending` without opening a leg (silent PnL / state corruption).
        if state.open.iter().any(|leg| leg.outcome == o) {
            continue;
        }
        let shares = (SIM_DEFAULT_SIZE_USDC / p.limit).max(0.01);
        if shares + 1e-9 < MIN_LIMIT_ORDER_SHARES {
            state.round.blocked_min_shares += 1;
            continue;
        }
        let Some(p) = state.pending.remove(&o) else {
            continue;
        };
        state.open.push(SimOpenLeg {
            outcome: o,
            entry: p.limit,
            shares,
        });
        state.round.entries_filled += 1;
    }
}

#[cfg(test)]
mod sim_fill_pending_tests {
    use super::*;

    fn snap_with_up_ask(ts: i64, up_ask: f64) -> SnapParsed {
        SnapParsed {
            _ts: ts,
            spot: None,
            ptb: None,
            sig: 0,
            sent: 3,
            ubu: Some(0.39),
            uba: Some(up_ask),
            dbu: Some(0.5),
            dba: Some(0.51),
            ubas: Some(10.0),
            dbas: Some(10.0),
            secs: Some(100),
            act: 50.0,
        }
    }

    #[test]
    fn fill_pending_keeps_resting_order_when_open_already_has_same_outcome() {
        let sim = SimTradingParams::from_cli_optional(None, None, None, None, None, None, None);
        let mut state = SimRoundState::default();
        state.pending.insert(
            Outcome::Up,
            PendingBuy {
                limit: 0.41,
                expires_ts: 2000,
            },
        );
        state.open.push(SimOpenLeg {
            outcome: Outcome::Up,
            entry: 0.40,
            shares: 12.0,
        });
        let snap = snap_with_up_ask(1000, 0.40);
        sim_try_fill_pending(&mut state, &snap, &sim);
        assert!(
            state.pending.contains_key(&Outcome::Up),
            "pending Up must remain when fill is skipped due to duplicate outcome in open"
        );
        assert_eq!(state.open.len(), 1);
        assert_eq!(state.round.entries_filled, 0);
    }

    #[test]
    fn fill_pending_keeps_resting_order_when_min_shares_blocks() {
        let sim = SimTradingParams::from_cli_optional(None, None, None, None, None, None, None);
        let mut state = SimRoundState::default();
        state.pending.insert(
            Outcome::Up,
            PendingBuy {
                limit: 2.0,
                expires_ts: 2000,
            },
        );
        let snap = snap_with_up_ask(1000, 1.90);
        sim_try_fill_pending(&mut state, &snap, &sim);
        assert!(
            state.pending.contains_key(&Outcome::Up),
            "pending must remain when share sizing is below MIN_LIMIT_ORDER_SHARES"
        );
        assert!(state.open.is_empty());
        assert_eq!(state.round.blocked_min_shares, 1);
    }
}

fn sim_process_exits(state: &mut SimRoundState, snap: &SnapParsed, sim: &SimTradingParams) {
    if state.open.is_empty() {
        return;
    }
    let mut still_open: Vec<SimOpenLeg> = Vec::new();
    for leg in state.open.drain(..) {
        let bid = snap_best_bid(snap, leg.outcome);
        let mut closed = false;
        if sim.stop_loss_bps > 0 {
            if let Some(b) = bid.filter(|x| x.is_finite()) {
                if stop_loss_triggered(leg.entry, b, sim.stop_loss_bps) {
                    let px = stop_loss_sell_limit_price(b);
                    state.round.exit_stop_loss += 1;
                    state.round.realized_pnl_usdc += (px - leg.entry) * leg.shares;
                    closed = true;
                }
            }
        }
        if !closed && sim.trailing_exit_min_profit_bps > 0 {
            if let Some(b) = bid.filter(|x| x.is_finite() && *x > 0.0) {
                let slip = SIM_DEFAULT_SELL_SLIPPAGE_BPS as f64 / 10_000.0;
                let sell_floor = clamp_prob(b * (1.0 - slip));
                if trailing_exit_sell_meets_min_gross_profit_bps(
                    sell_floor,
                    leg.entry,
                    sim.trailing_exit_min_profit_bps,
                ) {
                    state.round.exit_trailing_min_profit += 1;
                    state.round.realized_pnl_usdc += (sell_floor - leg.entry) * leg.shares;
                    closed = true;
                }
            }
        }
        if !closed {
            still_open.push(leg);
        }
    }
    state.open = still_open;
}

fn sim_try_enter(
    state: &mut SimRoundState,
    snap: &SnapParsed,
    replay: ManualSignalLabel,
    window_end_ts: i64,
    sim: &SimTradingParams,
) {
    let Some(outcome) = label_to_autotrading_outcome(replay) else {
        return;
    };
    state.round.strong_entry_signals += 1;
    if !sim_buy_time_gate_ok(
        snap.secs,
        sim.autotrading_buy_last_secs,
        sim.autotrading_buy_early_ptb_gap_bps,
        snap.spot,
        snap.ptb,
    ) {
        state.round.blocked_buy_last_secs += 1;
        return;
    }
    let Some(ask) = snap_best_ask(snap, outcome).filter(|a| a.is_finite() && *a > 0.0) else {
        return;
    };
    let limit = clamp_prob(ask);
    if sim
        .autotrading_max_entry_price
        .is_some_and(|cap| limit > cap + 1e-12)
    {
        state.round.blocked_max_entry_price += 1;
        return;
    }
    let shares = (SIM_DEFAULT_SIZE_USDC / limit).max(0.01);
    if shares + 1e-9 < MIN_LIMIT_ORDER_SHARES {
        state.round.blocked_min_shares += 1;
        return;
    }
    if state.open.iter().any(|leg| leg.outcome == outcome) {
        return;
    }
    if state.pending.contains_key(&outcome) {
        return;
    }
    if sim_reserved_count(&state.pending, &state.open) >= sim.autotrading_max_positions.max(1) {
        state.round.blocked_max_positions += 1;
        return;
    }
    let placed_ts = snap._ts;
    let expires_ts = sim_pending_expires_ts(placed_ts, sim.autotrading_order_expires_after_secs, window_end_ts);
    state.pending.insert(
        outcome,
        PendingBuy {
            limit,
            expires_ts,
        },
    );
}

fn sim_settle_round(state: &mut SimRoundState, win: &str) {
    for leg in state.open.drain(..) {
        let won = match leg.outcome {
            Outcome::Up => win == "up",
            Outcome::Down => win == "down",
        };
        let payoff = if won { 1.0 } else { 0.0 };
        if won {
            state.round.exit_settlement_win += 1;
        } else {
            state.round.exit_settlement_lose += 1;
        }
        state.round.realized_pnl_usdc += (payoff - leg.entry) * leg.shares;
    }
    let n = state.pending.len() as u64;
    state.round.unfilled_pending_at_round_close += n;
    state.pending.clear();
}

fn simulate_trading_round(
    snaps: &[SnapParsed],
    tun: &RoundLogStrategyTunables,
    ws: i64,
    we: i64,
    overrides: &TunableOverrides,
    sim: &SimTradingParams,
    strategy: SignalStrategy,
    win: &str,
) -> SimTradingSummary {
    let mut state = SimRoundState::default();
    for s in snaps {
        sim_try_fill_pending(&mut state, s, sim);
        sim_process_exits(&mut state, s, sim);
        let Some(input) = snap_to_input(s, tun, ws, we, overrides) else {
            continue;
        };
        let replay = evaluate_signal(strategy, &input);
        sim_try_enter(&mut state, s, replay, we, sim);
        sim_try_fill_pending(&mut state, s, sim);
    }
    sim_settle_round(&mut state, win);
    state.round
}

fn predicted_side_strong(label: ManualSignalLabel) -> Option<&'static str> {
    match label {
        ManualSignalLabel::StrongUp => Some("up"),
        ManualSignalLabel::StrongDown => Some("down"),
        _ => None,
    }
}

fn predicted_side_watch_hint(
    strategy: SignalStrategy,
    label: ManualSignalLabel,
    spot: Option<f64>,
    ptb: Option<f64>,
) -> Option<&'static str> {
    match label {
        ManualSignalLabel::StrongUp => Some("up"),
        ManualSignalLabel::StrongDown => Some("down"),
        ManualSignalLabel::Watch => {
            let (s, p) = (spot?, ptb?);
            match strategy {
                SignalStrategy::Rubric => {
                    if s > p {
                        Some("up")
                    } else if s < p {
                        Some("down")
                    } else {
                        None
                    }
                }
                SignalStrategy::CatchUp => {
                    if s > p {
                        Some("down")
                    } else if s < p {
                        Some("up")
                    } else {
                        None
                    }
                }
            }
        }
        ManualSignalLabel::NoTrade => None,
    }
}

fn sim_total_exits(sim: &SimTradingSummary) -> u64 {
    sim.exit_stop_loss
        + sim.exit_trailing_min_profit
        + sim.exit_settlement_win
        + sim.exit_settlement_lose
}

/// All closed legs exited via trailing take-profit or winning settlement (no stop-loss, no losing settlement).
fn sim_perfect_exit_win_rate(sim: &SimTradingSummary) -> bool {
    let n = sim_total_exits(sim);
    n > 0 && sim.exit_stop_loss == 0 && sim.exit_settlement_lose == 0
}

fn strong_replay_perfect(summary: &EvalSummary) -> bool {
    summary.strong_calls > 0 && summary.strong_correct == summary.strong_calls
}

/// Among sim-exit-perfect combos: prefer 100% strong replay vs settlement, then higher PnL.
fn is_better_tune_candidate(new: &EvalSummary, old: &EvalSummary) -> bool {
    let ns = u8::from(strong_replay_perfect(new));
    let os = u8::from(strong_replay_perfect(old));
    if ns != os {
        return ns > os;
    }
    new.sim_trading.realized_pnl_usdc > old.sim_trading.realized_pnl_usdc
}

fn strong_replay_win_rate(summary: &EvalSummary) -> Option<f64> {
    if summary.strong_calls == 0 {
        return None;
    }
    Some(summary.strong_correct as f64 / summary.strong_calls as f64)
}

fn sim_trading_win_rate(sim: &SimTradingSummary) -> Option<f64> {
    let n = sim_total_exits(sim);
    if n == 0 {
        return None;
    }
    let wins = sim.exit_trailing_min_profit + sim.exit_settlement_win;
    Some(wins as f64 / n as f64)
}

/// Requires [`sort_round_snaps_by_ts`] to have been applied to `rounds` (or snaps otherwise
/// chronologically sorted).
fn aggregate_signal_eval_on_rounds(
    rounds: &HashMap<String, RoundAccum>,
    mode: EvalMode,
    strategy: SignalStrategy,
    overrides: &TunableOverrides,
    sim_params: &SimTradingParams,
) -> EvalSummary {
    let rep = replay_strategy_metrics(rounds, mode, strategy, overrides);
    let mut summary = EvalSummary {
        rounds_total: rounds.len(),
        rounds_with_win: rep.rounds_with_win,
        snaps_total: rep.snaps_total,
        snaps_usable_book: rep.snaps_usable_book,
        mode: match mode {
            EvalMode::StrongOnly => "strong-only".into(),
            EvalMode::WatchAsHint => "watch-as-hint".into(),
        },
        strong_calls: rep.strong_calls,
        strong_correct: rep.strong_correct,
        watch_calls: rep.watch_calls,
        watch_correct: rep.watch_correct,
        replay_mismatch_live_sig: rep.replay_mismatch_live_sig,
        sim_trading: SimTradingSummary::default(),
        sim_trading_params: sim_params.clone(),
    };

    for (_cid, acc) in rounds {
        let Some(win) = acc.win.as_deref() else {
            continue;
        };
        if win != "up" && win != "down" {
            continue;
        }
        let Some(tun) = acc.tunables.as_ref() else {
            continue;
        };
        let ws = acc.ws.unwrap_or(0);
        let we = acc.we.unwrap_or(ws + 1).max(ws + 1);

        summary.sim_trading.merge(&simulate_trading_round(
            acc.snaps.as_slice(),
            tun,
            ws,
            we,
            overrides,
            sim_params,
            strategy,
            win,
        ));
    }

    summary
}

/// STRONG autotrading simulation on round logs using default `signal-eval` sim parameters
/// (same as omitting `--sim-*` overrides). Returns total **realized PnL in USDC** merged across
/// rounds with a resolved `up`/`down` winner.
pub fn replay_sim_realized_pnl_usdc(
    rounds: &HashMap<String, RoundAccum>,
    mode: EvalMode,
    strategy: SignalStrategy,
    overrides: &TunableOverrides,
) -> f64 {
    let sim = SimTradingParams::from_cli_optional(None, None, None, None, None, None, None);
    aggregate_signal_eval_on_rounds(rounds, mode, strategy, overrides, &sim).sim_trading.realized_pnl_usdc
}

fn aggregate_signal_eval(
    dir: &std::path::Path,
    day: Option<&str>,
    mode: EvalMode,
    strategy: SignalStrategy,
    overrides: &TunableOverrides,
    sim_params: &SimTradingParams,
) -> Result<Option<EvalSummary>> {
    let rounds = load_offline_rounds(dir, day)?;

    Ok(Some(aggregate_signal_eval_on_rounds(
        &rounds,
        mode,
        strategy,
        overrides,
        sim_params,
    )))
}

fn sim_trading_params_from_tuning_env() -> SimTradingParams {
    SimTradingParams::from_cli_optional(
        std::env::var("AUTOTRADING_BUY_LAST_SECS").ok().as_deref(),
        std::env::var("AUTOTRADING_BUY_EARLY_PTB_GAP_BPS")
            .ok()
            .as_deref(),
        std::env::var("AUTOTRADING_ORDER_EXPIRES_AFTER")
            .ok()
            .as_deref(),
        std::env::var("AUTOTRADING_MAX_ENTRY_PRICE").ok().as_deref(),
        std::env::var("AUTOTRADING_MAX_POSITIONS").ok().as_deref(),
        std::env::var("TRAILING_EXIT_MIN_PROFIT_BPS")
            .ok()
            .as_deref(),
        std::env::var("STOP_LOSS_BPS").ok().as_deref(),
    )
}

fn split_csv_grid(s: &str) -> Vec<&str> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .collect()
}

fn parse_grid_option_u64(token: &str) -> Result<Option<u64>> {
    let t = token.trim();
    if t.eq_ignore_ascii_case("none") || t == "-" {
        return Ok(None);
    }
    let n: u64 = t.parse().context("expected unsigned int or none")?;
    Ok(Some(n))
}

fn parse_grid_option_f64_prob(token: &str) -> Result<Option<f64>> {
    let t = token.trim();
    if t.eq_ignore_ascii_case("none") || t == "-" {
        return Ok(None);
    }
    let p: f64 = t.parse().context("max-entry price")?;
    if !p.is_finite() || !(0.01..=0.99).contains(&p) {
        bail!("max-entry price must be in 0.01..=0.99");
    }
    Ok(Some(p))
}

/// Upper bound for basis-point grids in `--grind bps` (inclusive).
const GRIND_BPS_INCLUSIVE_MAX: u32 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum GrindMode {
    /// Normal tuning: explicit `--grid-*` or single default per axis.
    #[default]
    None,
    /// Full `0..=GRIND_BPS_INCLUSIVE_MAX` for each sim bps axis whose `--grid-*` was omitted.
    Bps,
    /// Stepped exhaustive strategy overrides (no `log` / JSONL-only path) for omitted `--grid-strategy-*`.
    Strategy,
}

struct TuneAxes {
    strong_gap_mult: Vec<Option<f64>>,
    max_spread_mult: Vec<Option<f64>>,
    min_top_ask_shares: Vec<Option<f64>>,
    watch_ratio: Vec<Option<f64>>,
    buy_last_secs: Vec<Option<u64>>,
    early_ptb_gap_bps: Arc<[u32]>,
    order_expires_after_secs: Vec<Option<u64>>,
    max_entry_price: Vec<Option<f64>>,
    max_positions: Vec<usize>,
    trailing_exit_min_profit_bps: Arc<[u32]>,
    stop_loss_bps: Arc<[u32]>,
}

impl TuneAxes {
    fn combo_count_u64(&self) -> Option<u64> {
        let mut p: u64 = 1;
        for n in [
            self.strong_gap_mult.len() as u64,
            self.max_spread_mult.len() as u64,
            self.min_top_ask_shares.len() as u64,
            self.watch_ratio.len() as u64,
            self.buy_last_secs.len() as u64,
            self.early_ptb_gap_bps.len() as u64,
            self.order_expires_after_secs.len() as u64,
            self.max_entry_price.len() as u64,
            self.max_positions.len() as u64,
            self.trailing_exit_min_profit_bps.len() as u64,
            self.stop_loss_bps.len() as u64,
        ] {
            p = p.checked_mul(n)?;
        }
        Some(p)
    }

    fn for_each_combo(&self, mut f: impl FnMut(TunableOverrides, SimTradingParams)) {
        for &strong_gap_mult in &self.strong_gap_mult {
            for &max_spread_mult in &self.max_spread_mult {
                for &min_top_ask_shares in &self.min_top_ask_shares {
                    for &watch_ratio in &self.watch_ratio {
                        for &buy_last in &self.buy_last_secs {
                            for &early_ptb in self.early_ptb_gap_bps.iter() {
                                for &order_exp in &self.order_expires_after_secs {
                                    for &max_entry in &self.max_entry_price {
                                        for &max_pos in &self.max_positions {
                                            for &trail_bps in self.trailing_exit_min_profit_bps.iter() {
                                                for &sl_bps in self.stop_loss_bps.iter() {
                                                    let overrides = TunableOverrides {
                                                        strong_gap_mult,
                                                        max_spread_mult,
                                                        min_top_ask_shares,
                                                        watch_ratio,
                                                    };
                                                    let sim = SimTradingParams {
                                                        autotrading_buy_last_secs: buy_last,
                                                        autotrading_buy_early_ptb_gap_bps: early_ptb,
                                                        autotrading_order_expires_after_secs: order_exp,
                                                        autotrading_max_entry_price: max_entry,
                                                        autotrading_max_positions: max_pos,
                                                        trailing_exit_min_profit_bps: trail_bps,
                                                        stop_loss_bps: sl_bps,
                                                    };
                                                    f(overrides, sim);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn grind_bps_full_arc() -> Arc<[u32]> {
    Arc::from(
        (0..=GRIND_BPS_INCLUSIVE_MAX)
            .collect::<Vec<u32>>()
            .into_boxed_slice(),
    )
}

/// Overrides aligned with [`crate::config::parse_strategy_strong_gap_mult`] clamp range (0.55–1.15).
fn grind_strategy_strong_gap_somes() -> Vec<Option<f64>> {
    (55_i32..=115).map(|c| Some(c as f64 / 100.0)).collect()
}

/// 1.00–1.35 step 0.01 ([`crate::config::parse_strategy_max_spread_mult`]).
fn grind_strategy_max_spread_somes() -> Vec<Option<f64>> {
    (100_i32..=135).map(|c| Some(c as f64 / 100.0)).collect()
}

/// 2.0–50.0 step 0.5 shares ([`crate::config::parse_strategy_min_top_ask_shares`]).
fn grind_strategy_min_top_ask_somes() -> Vec<Option<f64>> {
    (4_i32..=100).map(|h| Some(h as f64 * 0.5)).collect()
}

/// 0.40–0.85 step 0.01 ([`crate::config::parse_strategy_watch_ratio`]).
fn grind_strategy_watch_ratio_somes() -> Vec<Option<f64>> {
    (40_i32..=85).map(|c| Some(c as f64 / 100.0)).collect()
}

fn parse_strategy_float_grid(arg: Option<&str>, label: &str) -> Result<Vec<Option<f64>>> {
    let Some(raw) = arg else {
        return Ok(vec![None]);
    };
    let mut out = Vec::new();
    for tok in split_csv_grid(raw) {
        if tok.eq_ignore_ascii_case("log") {
            out.push(None);
            continue;
        }
        let v: f64 = tok.parse().with_context(|| format!("{label}: {tok}"))?;
        if !v.is_finite() {
            bail!("{}: non-finite value", label);
        }
        out.push(Some(v));
    }
    if out.is_empty() {
        bail!("{}: empty grid", label);
    }
    Ok(out)
}

fn parse_usize_grid(arg: Option<&str>, label: &str, default: usize) -> Result<Vec<usize>> {
    let Some(raw) = arg else {
        return Ok(vec![default]);
    };
    let mut out = Vec::new();
    for tok in split_csv_grid(raw) {
        out.push(
            tok.parse::<usize>()
                .with_context(|| format!("{label}: {tok}"))?,
        );
    }
    if out.is_empty() {
        bail!("{}: empty grid", label);
    }
    Ok(out)
}

fn parse_option_u64_grid(arg: Option<&str>, label: &str) -> Result<Vec<Option<u64>>> {
    let Some(raw) = arg else {
        return Ok(vec![parse_autotrading_buy_last_secs(
            std::env::var("AUTOTRADING_BUY_LAST_SECS")
                .ok()
                .as_deref(),
        )]);
    };
    let mut out = Vec::new();
    for tok in split_csv_grid(raw) {
        out.push(parse_grid_option_u64(tok).with_context(|| format!("{label}: {tok}"))?);
    }
    if out.is_empty() {
        bail!("{}: empty grid", label);
    }
    Ok(out)
}

fn parse_option_u64_grid_order_expires(arg: Option<&str>, label: &str) -> Result<Vec<Option<u64>>> {
    let Some(raw) = arg else {
        return Ok(vec![parse_autotrading_order_expires_after_secs(
            std::env::var("AUTOTRADING_ORDER_EXPIRES_AFTER")
                .ok()
                .as_deref(),
        )]);
    };
    let mut out = Vec::new();
    for tok in split_csv_grid(raw) {
        out.push(parse_grid_option_u64(tok).with_context(|| format!("{label}: {tok}"))?);
    }
    if out.is_empty() {
        bail!("{}: empty grid", label);
    }
    Ok(out)
}

fn parse_option_f64_entry_grid(arg: Option<&str>, label: &str) -> Result<Vec<Option<f64>>> {
    let Some(raw) = arg else {
        return Ok(vec![parse_autotrading_max_entry_price(
            std::env::var("AUTOTRADING_MAX_ENTRY_PRICE")
                .ok()
                .as_deref(),
        )]);
    };
    let mut out = Vec::new();
    for tok in split_csv_grid(raw) {
        out.push(parse_grid_option_f64_prob(tok).with_context(|| format!("{label}: {tok}"))?);
    }
    if out.is_empty() {
        bail!("{}: empty grid", label);
    }
    Ok(out)
}

fn print_recommended_env_exports(overrides: &TunableOverrides, sim: &SimTradingParams) {
    println!("# Strategy (set empty/unset to use per-round log tunables for that key)");
    if let Some(v) = overrides.strong_gap_mult {
        println!("export STRATEGY_STRONG_GAP_MULT={v}");
    } else {
        println!("# STRATEGY_STRONG_GAP_MULT unset → per-round JSONL tunables");
    }
    if let Some(v) = overrides.max_spread_mult {
        println!("export STRATEGY_MAX_SPREAD_MULT={v}");
    } else {
        println!("# STRATEGY_MAX_SPREAD_MULT unset → per-round JSONL tunables");
    }
    if let Some(v) = overrides.min_top_ask_shares {
        println!("export STRATEGY_MIN_TOP_ASK_SHARES={v}");
    } else {
        println!("# STRATEGY_MIN_TOP_ASK_SHARES unset → per-round JSONL tunables");
    }
    if let Some(v) = overrides.watch_ratio {
        println!("export STRATEGY_WATCH_RATIO={v}");
    } else {
        println!("# STRATEGY_WATCH_RATIO unset → per-round JSONL tunables");
    }
    println!();
    if let Some(v) = sim.autotrading_buy_last_secs {
        println!("export AUTOTRADING_BUY_LAST_SECS={v}");
    } else {
        println!("# AUTOTRADING_BUY_LAST_SECS unset (no final-seconds buy window)");
    }
    println!(
        "export AUTOTRADING_BUY_EARLY_PTB_GAP_BPS={}",
        sim.autotrading_buy_early_ptb_gap_bps
    );
    if let Some(v) = sim.autotrading_order_expires_after_secs {
        println!("export AUTOTRADING_ORDER_EXPIRES_AFTER={v}");
    } else {
        println!("# AUTOTRADING_ORDER_EXPIRES_AFTER unset (resting until window end)");
    }
    if let Some(v) = sim.autotrading_max_entry_price {
        println!("export AUTOTRADING_MAX_ENTRY_PRICE={v}");
    } else {
        println!("# AUTOTRADING_MAX_ENTRY_PRICE unset");
    }
    println!(
        "export AUTOTRADING_MAX_POSITIONS={}",
        sim.autotrading_max_positions
    );
    println!(
        "export TRAILING_EXIT_MIN_PROFIT_BPS={}",
        sim.trailing_exit_min_profit_bps
    );
    println!("export STOP_LOSS_BPS={}", sim.stop_loss_bps);
}

/// Grid-search tuning (same logs as `signal-eval`). Picks combos with **100% simulated exit win
/// rate** (≥1 closed leg, no stop-loss exits, no losing settlements). Among those, prefers **100%
/// strong replay vs final winner**, then highest `realized_pnl_usdc`. If none qualify, falls back
/// to best sim exit win rate then PnL.
///
/// **`--grind`**: exhaustive default grids (no comma lists needed for those axes):
/// - **`--grind`** or **`--grind bps`**: each omitted `--grid-*` among early-PTB / trailing / stop-loss
///   bps is enumerated from **0 through 10_000** inclusive.
/// - **`--grind strategy`**: each omitted `--grid-strategy-*` uses a fixed stepped list matching config
///   clamp ranges (all `Some` overrides; no per-round `log` path).
/// Only one grind mode per run (`bps` and `strategy` are not combined — run twice for both).
///
/// **`--grind` requires `--day`** and an explicit **`--dir`** (not the implicit `./data/rounds` default).
/// **`--max-combos 0`** means unlimited (default when `--grind` is active unless you pass an explicit
/// **`--max-combos`**). **`--progress-every N`** prints stderr progress (default **100_000** when total
/// combos > 500_000 and `N` not set).
pub fn run_signal_eval_tune_cli(args: &[String]) -> Result<()> {
    let _ = dotenvy::dotenv();
    let mut dir = PathBuf::from("./data/rounds");
    let mut day: Option<String> = None;
    let mut mode = EvalMode::StrongOnly;
    let mut json_out = false;
    let mut max_combos: usize = 250_000;
    let mut max_combos_explicit = false;
    let mut grind_mode = GrindMode::None;
    let mut progress_every_cfg: u64 = 0;
    let mut dir_explicit = false;
    let mut strategy_cli: Option<String> = None;

    let mut g_strong_gap: Option<String> = None;
    let mut g_max_spread: Option<String> = None;
    let mut g_min_top_ask: Option<String> = None;
    let mut g_watch_ratio: Option<String> = None;
    let mut g_buy_last: Option<String> = None;
    let mut g_early_ptb: Option<String> = None;
    let mut g_order_exp: Option<String> = None;
    let mut g_max_entry: Option<String> = None;
    let mut g_max_pos: Option<String> = None;
    let mut g_trail: Option<String> = None;
    let mut g_stop: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--dir requires a path");
                };
                dir = PathBuf::from(p);
                dir_explicit = true;
            }
            "--day" => {
                i += 1;
                let Some(d) = args.get(i) else {
                    bail!("--day requires YYYY-MM-DD");
                };
                day = Some(d.clone());
            }
            "--mode" => {
                i += 1;
                let Some(m) = args.get(i) else {
                    bail!("--mode requires strong-only|watch-as-hint");
                };
                mode = EvalMode::parse(m).with_context(|| format!("unknown mode {m}"))?;
            }
            "--strategy" => {
                i += 1;
                let Some(s) = args.get(i) else {
                    bail!("--strategy requires rubric|catch-up");
                };
                strategy_cli = Some(s.clone());
            }
            "--json" => {
                json_out = true;
            }
            "--max-combos" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--max-combos requires a number (0 = unlimited)");
                };
                max_combos = x.parse().context("max-combos")?;
                max_combos_explicit = true;
            }
            "--grind" => {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    grind_mode = match args[i + 1].as_str() {
                        "bps" => GrindMode::Bps,
                        "strategy" => GrindMode::Strategy,
                        other => {
                            bail!("--grind: unknown mode '{other}'; use bps|strategy (or pass --grind alone for bps)")
                        }
                    };
                    i += 1;
                } else {
                    grind_mode = GrindMode::Bps;
                }
            }
            "--progress-every" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--progress-every requires a positive integer");
                };
                progress_every_cfg = x.parse().context("progress-every")?;
            }
            "--grid-strategy-strong-gap-mult" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-strategy-strong-gap-mult requires comma-separated floats or `log`");
                };
                g_strong_gap = Some(x.clone());
            }
            "--grid-strategy-max-spread-mult" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-strategy-max-spread-mult requires values");
                };
                g_max_spread = Some(x.clone());
            }
            "--grid-strategy-min-top-ask-shares" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-strategy-min-top-ask-shares requires values");
                };
                g_min_top_ask = Some(x.clone());
            }
            "--grid-strategy-watch-ratio" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-strategy-watch-ratio requires values");
                };
                g_watch_ratio = Some(x.clone());
            }
            "--grid-autotrading-buy-last-secs" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-autotrading-buy-last-secs requires comma list (none,60,…)");
                };
                g_buy_last = Some(x.clone());
            }
            "--grid-autotrading-buy-early-ptb-gap-bps" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-autotrading-buy-early-ptb-gap-bps requires values");
                };
                g_early_ptb = Some(x.clone());
            }
            "--grid-autotrading-order-expires-after" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-autotrading-order-expires-after requires values");
                };
                g_order_exp = Some(x.clone());
            }
            "--grid-autotrading-max-entry-price" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-autotrading-max-entry-price requires values");
                };
                g_max_entry = Some(x.clone());
            }
            "--grid-autotrading-max-positions" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-autotrading-max-positions requires values");
                };
                g_max_pos = Some(x.clone());
            }
            "--grid-trailing-exit-min-profit-bps" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-trailing-exit-min-profit-bps requires values");
                };
                g_trail = Some(x.clone());
            }
            "--grid-stop-loss-bps" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--grid-stop-loss-bps requires values");
                };
                g_stop = Some(x.clone());
            }
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    if grind_mode != GrindMode::None && !max_combos_explicit {
        max_combos = 0;
    }
    if grind_mode != GrindMode::None && day.is_none() {
        bail!("--grind requires --day YYYY-MM-DD (restrict to one UTC day of logs)");
    }
    if grind_mode != GrindMode::None && !dir_explicit {
        bail!("--grind requires an explicit --dir path (refuse default ./data/rounds)");
    }

    let base_sim = sim_trading_params_from_tuning_env();

    let strong_gap_mult = if grind_mode == GrindMode::Strategy && g_strong_gap.is_none() {
        grind_strategy_strong_gap_somes()
    } else {
        parse_strategy_float_grid(g_strong_gap.as_deref(), "grid-strategy-strong-gap-mult")?
    };
    let max_spread_mult = if grind_mode == GrindMode::Strategy && g_max_spread.is_none() {
        grind_strategy_max_spread_somes()
    } else {
        parse_strategy_float_grid(g_max_spread.as_deref(), "grid-strategy-max-spread-mult")?
    };
    let min_top_ask_shares = if grind_mode == GrindMode::Strategy && g_min_top_ask.is_none() {
        grind_strategy_min_top_ask_somes()
    } else {
        parse_strategy_float_grid(g_min_top_ask.as_deref(), "grid-strategy-min-top-ask-shares")?
    };
    let watch_ratio = if grind_mode == GrindMode::Strategy && g_watch_ratio.is_none() {
        grind_strategy_watch_ratio_somes()
    } else {
        parse_strategy_float_grid(g_watch_ratio.as_deref(), "grid-strategy-watch-ratio")?
    };

    let triple_bps_grind = grind_mode == GrindMode::Bps
        && g_early_ptb.is_none()
        && g_trail.is_none()
        && g_stop.is_none();
    let grind_bps_shared: Option<Arc<[u32]>> = triple_bps_grind.then(grind_bps_full_arc);

    let early_ptb_gap_bps = if grind_mode == GrindMode::Bps && g_early_ptb.is_none() {
        grind_bps_shared
            .as_ref()
            .map(|a| a.clone())
            .unwrap_or_else(grind_bps_full_arc)
    } else {
        parse_u32_axis_arc(
            g_early_ptb.as_deref(),
            "grid-autotrading-buy-early-ptb-gap-bps",
            base_sim.autotrading_buy_early_ptb_gap_bps,
            parse_autotrading_buy_early_ptb_gap_bps,
        )?
    };
    let trailing_exit_min_profit_bps = if grind_mode == GrindMode::Bps && g_trail.is_none() {
        grind_bps_shared
            .as_ref()
            .map(|a| a.clone())
            .unwrap_or_else(grind_bps_full_arc)
    } else {
        parse_u32_axis_arc(
            g_trail.as_deref(),
            "grid-trailing-exit-min-profit-bps",
            base_sim.trailing_exit_min_profit_bps,
            parse_trailing_exit_min_profit_bps,
        )?
    };
    let stop_loss_bps = if grind_mode == GrindMode::Bps && g_stop.is_none() {
        grind_bps_shared
            .as_ref()
            .map(|a| a.clone())
            .unwrap_or_else(grind_bps_full_arc)
    } else {
        parse_u32_axis_arc(
            g_stop.as_deref(),
            "grid-stop-loss-bps",
            base_sim.stop_loss_bps,
            parse_stop_loss_bps,
        )?
    };

    let axes = TuneAxes {
        strong_gap_mult,
        max_spread_mult,
        min_top_ask_shares,
        watch_ratio,
        buy_last_secs: parse_option_u64_grid(g_buy_last.as_deref(), "grid-autotrading-buy-last-secs")?,
        early_ptb_gap_bps,
        order_expires_after_secs: parse_option_u64_grid_order_expires(
            g_order_exp.as_deref(),
            "grid-autotrading-order-expires-after",
        )?,
        max_entry_price: parse_option_f64_entry_grid(g_max_entry.as_deref(), "grid-autotrading-max-entry-price")?,
        max_positions: parse_usize_grid(
            g_max_pos.as_deref(),
            "grid-autotrading-max-positions",
            base_sim.autotrading_max_positions,
        )?,
        trailing_exit_min_profit_bps,
        stop_loss_bps,
    };

    let n = axes
        .combo_count_u64()
        .context("grid Cartesian product overflowed u64 (too many axes)")?;
    if max_combos > 0 && n > max_combos as u64 {
        bail!(
            "grid Cartesian product is {n} combos (limit {max_combos}); narrow grids, use a different --grind mode, or pass --max-combos 0 for unlimited"
        );
    }

    let progress_every = if progress_every_cfg > 0 {
        progress_every_cfg
    } else if n > 500_000 {
        100_000
    } else {
        0
    };
    if grind_mode != GrindMode::None {
        eprintln!(
            "signal-eval-tune: grind mode {:?} — {n} combinations{}",
            grind_mode,
            if progress_every > 0 {
                format!(" (progress every {progress_every})")
            } else {
                String::new()
            }
        );
    }

    let paths = list_jsonl_round_paths(&dir, day.as_deref())?;
    if paths.is_empty() {
        println!("No .jsonl files under {}", dir.display());
        return Ok(());
    }

    let mut rounds: HashMap<String, RoundAccum> = HashMap::new();
    for p in &paths {
        ingest_jsonl(p, &mut rounds)?;
    }
    sort_round_snaps_by_ts(&mut rounds);

    let strategy = signal_strategy_cli_or_env(strategy_cli.as_deref())?;

    let mut best_perfect: Option<(EvalSummary, TunableOverrides, SimTradingParams)> = None;
    let mut best_fallback: Option<(EvalSummary, TunableOverrides, SimTradingParams)> = None;
    let mut best_fallback_rate: f64 = -1.0;

    let mut done: u64 = 0;
    axes.for_each_combo(|overrides, sim| {
        done += 1;
        if progress_every > 0 && done % progress_every == 0 {
            eprintln!(
                "signal-eval-tune: progress {done}/{n} ({:.2}%)",
                100.0 * done as f64 / n as f64
            );
        }
        let summary = aggregate_signal_eval_on_rounds(&rounds, mode, strategy, &overrides, &sim);
        let sim_sr = sim_trading_win_rate(&summary.sim_trading).unwrap_or(-1.0);
        let perfect = sim_perfect_exit_win_rate(&summary.sim_trading);

        if perfect {
            let replace = best_perfect
                .as_ref()
                .map_or(true, |(s, _, _)| is_better_tune_candidate(&summary, s));
            if replace {
                best_perfect = Some((summary, overrides, sim));
            }
        } else if sim_sr > best_fallback_rate + 1e-15
            || (f64::abs(sim_sr - best_fallback_rate) <= 1e-15
                && best_fallback.as_ref().map_or(true, |(s, _, _)| {
                    summary.sim_trading.realized_pnl_usdc > s.sim_trading.realized_pnl_usdc
                }))
        {
            best_fallback_rate = sim_sr;
            best_fallback = Some((summary, overrides.clone(), sim.clone()));
        }
    });

    let picked_perfect = best_perfect.is_some();
    let (winner_summary, winner_o, winner_s) = best_perfect
        .or(best_fallback)
        .context("internal: no grid combo evaluated")?;

    let sim_wr = sim_trading_win_rate(&winner_summary.sim_trading);
    let strong_wr = strong_replay_win_rate(&winner_summary);

    if json_out {
        #[derive(Serialize)]
        struct TuneJsonOut {
            picked_perfect_exit_win_rate: bool,
            sim_exit_win_rate: Option<f64>,
            strong_replay_win_rate: Option<f64>,
            realized_pnl_usdc: f64,
            summary: EvalSummary,
        }
        let out = TuneJsonOut {
            picked_perfect_exit_win_rate: picked_perfect,
            sim_exit_win_rate: sim_wr,
            strong_replay_win_rate: strong_wr,
            realized_pnl_usdc: winner_summary.sim_trading.realized_pnl_usdc,
            summary: winner_summary,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if picked_perfect {
        println!("signal-eval-tune: best combo (100% sim exit win rate; tie-break: 100% strong replay vs win, then highest PnL)");
    } else {
        println!("signal-eval-tune: no combo achieved 100% sim exit win rate; showing best exit win rate then PnL");
    }
    println!(
        "  evaluated {n} combinations on {} rounds ({} with up/down win)",
        winner_summary.rounds_total, winner_summary.rounds_with_win
    );
    println!(
        "  sim exit win rate: {:.4}%  |  strong replay vs win: {}",
        sim_wr.map(|r| 100.0 * r).unwrap_or(f64::NAN),
        strong_wr
            .map(|r| format!("{:.4}%", 100.0 * r))
            .unwrap_or_else(|| "n/a (no strong calls)".into())
    );
    println!(
        "  realized PnL (USDC est.): {:.4}",
        winner_summary.sim_trading.realized_pnl_usdc
    );
    println!();
    println!("Recommended environment (matches .env names used by config + strategy):");
    print_recommended_env_exports(&winner_o, &winner_s);

    Ok(())
}

fn parse_u32_axis_arc(
    arg: Option<&str>,
    label: &str,
    default: u32,
    parse_one: fn(Option<&str>) -> u32,
) -> Result<Arc<[u32]>> {
    let v: Vec<u32> = if let Some(raw) = arg {
        let mut out = Vec::new();
        for tok in split_csv_grid(raw) {
            out.push(parse_one(Some(tok)));
        }
        if out.is_empty() {
            bail!("{}: empty grid", label);
        }
        out
    } else {
        vec![default]
    };
    Ok(Arc::from(v.into_boxed_slice()))
}

/// Run `signal-eval` CLI (args after subcommand name).
pub fn run_signal_eval_cli(args: &[String]) -> Result<()> {
    let _ = dotenvy::dotenv();
    let mut dir = PathBuf::from("./data/rounds");
    let mut day: Option<String> = None;
    let mut mode = EvalMode::StrongOnly;
    let mut json_out = false;
    let mut strategy_cli: Option<String> = None;
    let mut overrides = TunableOverrides {
        strong_gap_mult: None,
        max_spread_mult: None,
        min_top_ask_shares: None,
        watch_ratio: None,
    };

    let mut sim_buy_last_secs: Option<String> = None;
    let mut sim_early_ptb_gap_bps: Option<String> = None;
    let mut sim_order_expires_after: Option<String> = None;
    let mut sim_max_entry_price: Option<String> = None;
    let mut sim_max_positions: Option<String> = None;
    let mut sim_trailing_exit_min_profit_bps: Option<String> = None;
    let mut sim_stop_loss_bps: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    bail!("--dir requires a path");
                };
                dir = PathBuf::from(p);
            }
            "--day" => {
                i += 1;
                let Some(d) = args.get(i) else {
                    bail!("--day requires YYYY-MM-DD");
                };
                day = Some(d.clone());
            }
            "--mode" => {
                i += 1;
                let Some(m) = args.get(i) else {
                    bail!("--mode requires strong-only|watch-as-hint");
                };
                mode = EvalMode::parse(m).with_context(|| format!("unknown mode {m}"))?;
            }
            "--strategy" => {
                i += 1;
                let Some(s) = args.get(i) else {
                    bail!("--strategy requires rubric|catch-up");
                };
                strategy_cli = Some(s.clone());
            }
            "--json" => {
                json_out = true;
            }
            "--strong-gap-mult" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--strong-gap-mult requires a number");
                };
                overrides.strong_gap_mult = Some(x.parse().context("strong-gap-mult")?);
            }
            "--max-spread-mult" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--max-spread-mult requires a number");
                };
                overrides.max_spread_mult = Some(x.parse().context("max-spread-mult")?);
            }
            "--min-top-ask-shares" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--min-top-ask-shares requires a number");
                };
                overrides.min_top_ask_shares = Some(x.parse().context("min-top-ask-shares")?);
            }
            "--watch-ratio" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--watch-ratio requires a number");
                };
                overrides.watch_ratio = Some(x.parse().context("watch-ratio")?);
            }
            "--autotrading-buy-last-secs" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--autotrading-buy-last-secs requires seconds (same semantics as env; 0 = disabled)");
                };
                sim_buy_last_secs = Some(x.clone());
            }
            "--autotrading-buy-early-ptb-gap-bps" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--autotrading-buy-early-ptb-gap-bps requires a number");
                };
                sim_early_ptb_gap_bps = Some(x.clone());
            }
            "--autotrading-order-expires-after" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--autotrading-order-expires-after requires seconds (positive) or 0 for unset default");
                };
                sim_order_expires_after = Some(x.clone());
            }
            "--autotrading-max-entry-price" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--autotrading-max-entry-price requires a probability price 0.01–0.99");
                };
                sim_max_entry_price = Some(x.clone());
            }
            "--autotrading-max-positions" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--autotrading-max-positions requires a positive integer");
                };
                sim_max_positions = Some(x.clone());
            }
            "--trailing-exit-min-profit-bps" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--trailing-exit-min-profit-bps requires a number");
                };
                sim_trailing_exit_min_profit_bps = Some(x.clone());
            }
            "--stop-loss-bps" => {
                i += 1;
                let Some(x) = args.get(i) else {
                    bail!("--stop-loss-bps requires a number");
                };
                sim_stop_loss_bps = Some(x.clone());
            }
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    let sim_params = SimTradingParams::from_cli_optional(
        sim_buy_last_secs.as_deref(),
        sim_early_ptb_gap_bps.as_deref(),
        sim_order_expires_after.as_deref(),
        sim_max_entry_price.as_deref(),
        sim_max_positions.as_deref(),
        sim_trailing_exit_min_profit_bps.as_deref(),
        sim_stop_loss_bps.as_deref(),
    );

    let strategy = signal_strategy_cli_or_env(strategy_cli.as_deref())?;

    let Some(summary) = aggregate_signal_eval(
        &dir,
        day.as_deref(),
        mode,
        strategy,
        &overrides,
        &sim_params,
    )?
    else {
        println!("No .jsonl files under {}", dir.display());
        return Ok(());
    };

    if json_out {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("signal-eval summary ({})", summary.mode);
        println!("  strategy: {}", strategy_label(strategy));
        println!("  rounds (total): {}", summary.rounds_total);
        println!("  rounds with win up/down: {}", summary.rounds_with_win);
        println!("  snaps total: {}", summary.snaps_total);
        println!(
            "  snaps with full book (replayable): {}",
            summary.snaps_usable_book
        );
        println!("  strong calls (replay): {}", summary.strong_calls);
        if summary.strong_calls > 0 {
            println!(
                "  strong accuracy vs win: {:.2}% ({}/{})",
                100.0 * summary.strong_correct as f64 / summary.strong_calls as f64,
                summary.strong_correct,
                summary.strong_calls
            );
        }
        if mode == EvalMode::WatchAsHint {
            println!("  watch calls (replay): {}", summary.watch_calls);
            if summary.watch_calls > 0 {
                println!(
                    "  watch-as-hint accuracy vs win: {:.2}% ({}/{})",
                    100.0 * summary.watch_correct as f64 / summary.watch_calls as f64,
                    summary.watch_correct,
                    summary.watch_calls
                );
            }
        }
        println!(
            "  replay label != logged sig (sanity): {}",
            summary.replay_mismatch_live_sig
        );
        println!("  sim (autotrading-style, STRONG entries only):");
        println!(
            "    params: buy_last_secs={:?} early_ptb_gap_bps={} order_expires_after_secs={:?} max_entry_price={:?} max_positions={} trailing_exit_min_profit_bps={} stop_loss_bps={}",
            summary.sim_trading_params.autotrading_buy_last_secs,
            summary.sim_trading_params.autotrading_buy_early_ptb_gap_bps,
            summary.sim_trading_params.autotrading_order_expires_after_secs,
            summary.sim_trading_params.autotrading_max_entry_price,
            summary.sim_trading_params.autotrading_max_positions,
            summary.sim_trading_params.trailing_exit_min_profit_bps,
            summary.sim_trading_params.stop_loss_bps,
        );
        println!(
            "    strong entry signals: {}",
            summary.sim_trading.strong_entry_signals
        );
        println!("    entries filled: {}", summary.sim_trading.entries_filled);
        println!(
            "    blocked (time / max entry / max pos / min shares): {} / {} / {} / {}",
            summary.sim_trading.blocked_buy_last_secs,
            summary.sim_trading.blocked_max_entry_price,
            summary.sim_trading.blocked_max_positions,
            summary.sim_trading.blocked_min_shares
        );
        println!(
            "    pending expired mid-round / unfilled at close: {} / {}",
            summary.sim_trading.pending_buy_expired,
            summary.sim_trading.unfilled_pending_at_round_close
        );
        println!(
            "    exits — stop-loss / min-profit / settle win / settle lose: {} / {} / {} / {}",
            summary.sim_trading.exit_stop_loss,
            summary.sim_trading.exit_trailing_min_profit,
            summary.sim_trading.exit_settlement_win,
            summary.sim_trading.exit_settlement_lose
        );
        println!(
            "    realized PnL (USDC est., {:.0} USDC notional per entry): {:.4}",
            SIM_DEFAULT_SIZE_USDC,
            summary.sim_trading.realized_pnl_usdc
        );
    }

    Ok(())
}

#[cfg(test)]
mod signal_eval_tests {
    use std::collections::HashMap;
    use std::io::Write;

    use super::{
        aggregate_signal_eval_on_rounds, ingest_jsonl, list_jsonl_round_paths,
        load_offline_rounds_filtered, replay_strategy_metrics, simulate_trading_round, snap_to_input,
        EvalMode, RoundAccum, SimTradingParams, TunableOverrides,
    };
    use crate::config::SignalStrategy;
    use crate::strategy::{evaluate_manual_signal, ManualSignalLabel};

    #[test]
    fn day_filter_includes_profile_suffixed_jsonl_files() {
        let dir =
            std::env::temp_dir().join(format!("signal_eval_day_filter_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-05-11.jsonl"), "").unwrap();
        std::fs::write(dir.join("2026-05-11-btc-5m.jsonl"), "").unwrap();
        std::fs::write(dir.join("2026-05-12-btc-5m.jsonl"), "").unwrap();

        let paths = list_jsonl_round_paths(&dir, Some("2026-05-11")).unwrap();

        assert_eq!(paths.len(), 2);
        assert!(paths.iter().any(|p| p.ends_with("2026-05-11.jsonl")));
        assert!(paths.iter().any(|p| p.ends_with("2026-05-11-btc-5m.jsonl")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_offline_rounds_filtered_errors_without_profile_when_multiple_match_day() {
        let dir = std::env::temp_dir().join(format!(
            "signal_eval_filtered_multi_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-05-11-btc-5m.jsonl"), "").unwrap();
        std::fs::write(dir.join("2026-05-11-sol-5m.jsonl"), "").unwrap();
        match load_offline_rounds_filtered(&dir, Some("2026-05-11"), None) {
            Err(e) => assert!(e.to_string().contains("--profile"), "{e}"),
            Ok(_) => panic!("expected error without --profile"),
        }
        let (rounds, paths) =
            load_offline_rounds_filtered(&dir, Some("2026-05-11"), Some("sol-5m")).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("2026-05-11-sol-5m.jsonl"));
        assert!(rounds.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_metrics_match_aggregate_replay_fields() {
        let dir = std::env::temp_dir().join(format!(
            "replay_metrics_agg_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-05-10.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"t":"open","v":1,"cid":"c1","slug":"s","ws":1000,"we":1300,"ptb":100.0,"up":"u","down":"d","asset":"BTC","strong_gap_mult":1.0,"max_spread_mult":1.0,"min_top_ask_shares":5.0,"watch_ratio":0.6}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"snap","v":1,"cid":"c1","ts":1100,"spot":100.5,"ptb":100.0,"sig":0,"sent":3,"ubu":0.4,"uba":0.41,"dbu":0.58,"dba":0.59,"ubas":10.0,"dbas":10.0,"secs":200,"act":50.0}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"close","v":1,"cid":"c1","ts_close":1300,"spot_last":100.6,"win":"up","src":"approx_spot"}}"#
        )
        .unwrap();

        let mut rounds: HashMap<String, RoundAccum> = HashMap::new();
        ingest_jsonl(&path, &mut rounds).unwrap();
        for acc in rounds.values_mut() {
            acc.snaps.sort_by_key(|s| s._ts);
        }
        let overrides = TunableOverrides::default();
        let sim = SimTradingParams::from_cli_optional(None, None, None, None, None, None, None);
        let rep = replay_strategy_metrics(
            &rounds,
            EvalMode::StrongOnly,
            SignalStrategy::Rubric,
            &overrides,
        );
        let summary = aggregate_signal_eval_on_rounds(
            &rounds,
            EvalMode::StrongOnly,
            SignalStrategy::Rubric,
            &overrides,
            &sim,
        );
        assert_eq!(summary.rounds_with_win, rep.rounds_with_win);
        assert_eq!(summary.snaps_total, rep.snaps_total);
        assert_eq!(summary.snaps_usable_book, rep.snaps_usable_book);
        assert_eq!(summary.strong_calls, rep.strong_calls);
        assert_eq!(summary.strong_correct, rep.strong_correct);
        assert_eq!(summary.watch_calls, rep.watch_calls);
        assert_eq!(summary.watch_correct, rep.watch_correct);
        assert_eq!(
            summary.replay_mismatch_live_sig,
            rep.replay_mismatch_live_sig
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strong_gap_mult_monotonic_on_fixture() {
        let dir = std::env::temp_dir().join(format!("round_log_eval_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-05-10.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"t":"open","v":1,"cid":"c1","slug":"s","ws":1000,"we":1300,"ptb":100.0,"up":"u","down":"d","asset":"BTC","strong_gap_mult":1.0,"max_spread_mult":1.0,"min_top_ask_shares":5.0,"watch_ratio":0.6}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"snap","v":1,"cid":"c1","ts":1100,"spot":100.5,"ptb":100.0,"sig":0,"sent":3,"ubu":0.4,"uba":0.41,"dbu":0.58,"dba":0.59,"ubas":10.0,"dbas":10.0,"secs":200,"act":50.0}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"close","v":1,"cid":"c1","ts_close":1300,"spot_last":100.6,"win":"up","src":"approx_spot"}}"#
        )
        .unwrap();

        let mut rounds: HashMap<String, RoundAccum> = HashMap::new();
        ingest_jsonl(&path, &mut rounds).unwrap();
        let acc = rounds.get("c1").expect("round c1");
        let tun = acc.tunables.as_ref().unwrap();
        let ws = acc.ws.unwrap();
        let we = acc.we.unwrap();
        let snap = &acc.snaps[0];

        let low = snap_to_input(
            snap,
            tun,
            ws,
            we,
            &TunableOverrides {
                strong_gap_mult: Some(0.55),
                max_spread_mult: None,
                min_top_ask_shares: None,
                watch_ratio: None,
            },
        )
        .unwrap();
        let high = snap_to_input(
            snap,
            tun,
            ws,
            we,
            &TunableOverrides {
                strong_gap_mult: Some(1.15),
                max_spread_mult: None,
                min_top_ask_shares: None,
                watch_ratio: None,
            },
        )
        .unwrap();
        let a = evaluate_manual_signal(&low);
        let b = evaluate_manual_signal(&high);
        let rank = |l: ManualSignalLabel| match l {
            ManualSignalLabel::NoTrade => 0,
            ManualSignalLabel::Watch => 1,
            ManualSignalLabel::StrongUp | ManualSignalLabel::StrongDown => 2,
        };
        assert!(
            rank(a) >= rank(b),
            "lower strong_gap_mult should not reduce signal strength class"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sim_respects_autotrading_buy_last_secs() {
        let dir = std::env::temp_dir().join(format!("sim_buy_last_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("day.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"t":"open","v":1,"cid":"c1","ws":1000,"we":3000,"strong_gap_mult":1.0,"max_spread_mult":1.0,"min_top_ask_shares":5.0,"watch_ratio":0.6}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"snap","v":1,"cid":"c1","ts":1100,"spot":101.0,"ptb":100.0,"sig":0,"sent":3,"ubu":0.4,"uba":0.41,"dbu":0.58,"dba":0.59,"ubas":10.0,"dbas":10.0,"secs":200,"act":1.0}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"t":"snap","v":1,"cid":"c1","ts":1200,"spot":101.0,"ptb":100.0,"sig":0,"sent":3,"ubu":0.4,"uba":0.41,"dbu":0.58,"dba":0.59,"ubas":10.0,"dbas":10.0,"secs":50,"act":1.0}}"#
        )
        .unwrap();
        writeln!(f, r#"{{"t":"close","v":1,"cid":"c1","win":"up"}}"#).unwrap();

        let mut rounds: HashMap<String, RoundAccum> = HashMap::new();
        ingest_jsonl(&path, &mut rounds).unwrap();
        let acc = rounds.get("c1").expect("round c1");
        let tun = acc.tunables.as_ref().unwrap();
        let ws = acc.ws.unwrap();
        let we = acc.we.unwrap();
        let mut snaps = acc.snaps.clone();
        snaps.sort_by_key(|s| s._ts);

        let sim = SimTradingParams::from_cli_optional(
            Some("60"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let s = simulate_trading_round(
            &snaps,
            tun,
            ws,
            we,
            &TunableOverrides::default(),
            &sim,
            SignalStrategy::Rubric,
            "up",
        );
        assert!(
            s.blocked_buy_last_secs >= 1,
            "expected at least one STRONG snap outside the final 60s to be time-blocked"
        );
        assert_eq!(
            s.entries_filled, 1,
            "expected one fill once secs_to_close enters the window"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grind_bps_axis_covers_zero_through_max_inclusive() {
        let a = super::grind_bps_full_arc();
        assert_eq!(a.len(), 10_001);
        assert_eq!(a.first().copied(), Some(0));
        assert_eq!(a.last().copied(), Some(super::GRIND_BPS_INCLUSIVE_MAX));
    }

    #[test]
    fn grind_strategy_axes_cartesian_is_finite_millions() {
        let axes = super::TuneAxes {
            strong_gap_mult: super::grind_strategy_strong_gap_somes(),
            max_spread_mult: super::grind_strategy_max_spread_somes(),
            min_top_ask_shares: super::grind_strategy_min_top_ask_somes(),
            watch_ratio: super::grind_strategy_watch_ratio_somes(),
            buy_last_secs: vec![None],
            early_ptb_gap_bps: std::sync::Arc::from([0_u32]),
            order_expires_after_secs: vec![None],
            max_entry_price: vec![None],
            max_positions: vec![1],
            trailing_exit_min_profit_bps: std::sync::Arc::from([0_u32]),
            stop_loss_bps: std::sync::Arc::from([0_u32]),
        };
        let n = axes.combo_count_u64().expect("combo count");
        assert_eq!(n, 9_798_552);
    }

    #[test]
    fn predicted_side_watch_hint_catch_up_inverts_rubric_for_watch() {
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::Rubric,
                ManualSignalLabel::Watch,
                Some(101.0),
                Some(100.0),
            ),
            Some("up")
        );
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::CatchUp,
                ManualSignalLabel::Watch,
                Some(101.0),
                Some(100.0),
            ),
            Some("down")
        );
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::Rubric,
                ManualSignalLabel::Watch,
                Some(99.0),
                Some(100.0),
            ),
            Some("down")
        );
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::CatchUp,
                ManualSignalLabel::Watch,
                Some(99.0),
                Some(100.0),
            ),
            Some("up")
        );
    }

    #[test]
    fn predicted_side_watch_hint_returns_none_without_spot_or_ptb() {
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::CatchUp,
                ManualSignalLabel::Watch,
                None,
                Some(100.0),
            ),
            None
        );
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::CatchUp,
                ManualSignalLabel::Watch,
                Some(101.0),
                None,
            ),
            None
        );
    }

    #[test]
    fn predicted_side_watch_hint_equal_spot_ptb_is_none() {
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::CatchUp,
                ManualSignalLabel::Watch,
                Some(100.0),
                Some(100.0),
            ),
            None
        );
    }

    #[test]
    fn predicted_side_watch_hint_strong_labels_ignore_strategy() {
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::Rubric,
                ManualSignalLabel::StrongUp,
                Some(100.0),
                Some(100.0),
            ),
            Some("up")
        );
        assert_eq!(
            super::predicted_side_watch_hint(
                SignalStrategy::CatchUp,
                ManualSignalLabel::StrongDown,
                Some(100.0),
                Some(100.0),
            ),
            Some("down")
        );
    }
}
