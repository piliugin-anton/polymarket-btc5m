//! Offline replay of [`crate::strategy::evaluate_manual_signal`] on JSONL `snap` rows, plus an
//! optional **STRONG-only** autotrading-style simulation (time / max entry / max positions / GTD
//! TTL, stop-loss, trailing min-profit exit) using the same env defaults as [`crate::config::Config`]
//! when CLI flags are omitted.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::app::{
    clamp_prob, trailing_exit_sell_meets_min_gross_profit_bps, Outcome, MIN_LIMIT_ORDER_SHARES,
};
use crate::config::{
    parse_autotrading_buy_early_ptb_gap_bps, parse_autotrading_buy_last_secs,
    parse_autotrading_max_entry_price, parse_autotrading_max_positions,
    parse_autotrading_order_expires_after_secs, parse_stop_loss_bps,
    parse_trailing_exit_min_profit_bps,
};
use crate::round_log::{u8_to_label, u8_to_sentiment, RoundLogStrategyTunables};
use crate::stop_loss::{stop_loss_sell_limit_price, stop_loss_triggered};
use crate::strategy::{
    evaluate_manual_signal, ManualSignalBookSide, ManualSignalInput, ManualSignalLabel,
};

/// Default USDC size for simulated autotrading entries (matches `DEFAULT_SIZE_USDC` in config).
const SIM_DEFAULT_SIZE_USDC: f64 = 5.0;
/// Default sell slippage bps for simulated trailing-floor checks (matches `MARKET_SELL_SLIPPAGE_BPS`).
const SIM_DEFAULT_SELL_SLIPPAGE_BPS: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalMode {
    StrongOnly,
    WatchAsHint,
}

impl EvalMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "strong-only" => Some(Self::StrongOnly),
            "watch-as-hint" => Some(Self::WatchAsHint),
            _ => None,
        }
    }
}

#[derive(Default)]
struct RoundAccum {
    ws: Option<i64>,
    we: Option<i64>,
    tunables: Option<RoundLogStrategyTunables>,
    snaps: Vec<SnapParsed>,
    win: Option<String>,
}

#[derive(Clone)]
struct SnapParsed {
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

#[derive(Serialize)]
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

fn list_jsonl_files(dir: &std::path::Path, day: Option<&str>) -> Result<Vec<PathBuf>> {
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

#[derive(Default)]
struct TunableOverrides {
    strong_gap_mult: Option<f64>,
    max_spread_mult: Option<f64>,
    min_top_ask_shares: Option<f64>,
    watch_ratio: Option<f64>,
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

fn sim_try_fill_pending(state: &mut SimRoundState, snap: &SnapParsed, sim: &SimTradingParams) {
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
        let Some(p) = state.pending.remove(&o) else {
            continue;
        };
        if sim_reserved_count(&state.pending, &state.open) >= sim.autotrading_max_positions.max(1) {
            state.round.blocked_max_positions += 1;
            continue;
        }
        if state.open.iter().any(|leg| leg.outcome == o) {
            continue;
        }
        let shares = (SIM_DEFAULT_SIZE_USDC / p.limit).max(0.01);
        if shares + 1e-9 < MIN_LIMIT_ORDER_SHARES {
            state.round.blocked_min_shares += 1;
            continue;
        }
        state.open.push(SimOpenLeg {
            outcome: o,
            entry: p.limit,
            shares,
        });
        state.round.entries_filled += 1;
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
    win: &str,
) -> SimTradingSummary {
    let mut state = SimRoundState::default();
    for s in snaps {
        sim_try_fill_pending(&mut state, s, sim);
        sim_process_exits(&mut state, s, sim);
        let Some(input) = snap_to_input(s, tun, ws, we, overrides) else {
            continue;
        };
        let replay = evaluate_manual_signal(&input);
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
    label: ManualSignalLabel,
    spot: Option<f64>,
    ptb: Option<f64>,
) -> Option<&'static str> {
    match label {
        ManualSignalLabel::StrongUp => Some("up"),
        ManualSignalLabel::StrongDown => Some("down"),
        ManualSignalLabel::Watch => {
            let (s, p) = (spot?, ptb?);
            if s > p {
                Some("up")
            } else if s < p {
                Some("down")
            } else {
                None
            }
        }
        ManualSignalLabel::NoTrade => None,
    }
}

/// Run `signal-eval` CLI (args after subcommand name).
pub fn run_signal_eval_cli(args: &[String]) -> Result<()> {
    let mut dir = PathBuf::from("./data/rounds");
    let mut day: Option<String> = None;
    let mut mode = EvalMode::StrongOnly;
    let mut json_out = false;
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

    let paths = list_jsonl_files(&dir, day.as_deref())?;
    if paths.is_empty() {
        println!("No .jsonl files under {}", dir.display());
        return Ok(());
    }

    let mut rounds: HashMap<String, RoundAccum> = HashMap::new();
    for p in &paths {
        ingest_jsonl(p, &mut rounds)?;
    }

    let mut summary = EvalSummary {
        rounds_total: rounds.len(),
        rounds_with_win: 0,
        snaps_total: 0,
        snaps_usable_book: 0,
        mode: match mode {
            EvalMode::StrongOnly => "strong-only".into(),
            EvalMode::WatchAsHint => "watch-as-hint".into(),
        },
        strong_calls: 0,
        strong_correct: 0,
        watch_calls: 0,
        watch_correct: 0,
        replay_mismatch_live_sig: 0,
        sim_trading: SimTradingSummary::default(),
        sim_trading_params: sim_params.clone(),
    };

    for (_cid, acc) in &rounds {
        let Some(win) = acc.win.as_deref() else {
            continue;
        };
        if win != "up" && win != "down" {
            continue;
        }
        summary.rounds_with_win += 1;
        let Some(tun) = acc.tunables.as_ref() else {
            continue;
        };
        let ws = acc.ws.unwrap_or(0);
        let we = acc.we.unwrap_or(ws + 1).max(ws + 1);

        let mut snaps_sorted = acc.snaps.clone();
        snaps_sorted.sort_by_key(|s| s._ts);
        summary.sim_trading.merge(&simulate_trading_round(
            &snaps_sorted,
            tun,
            ws,
            we,
            &overrides,
            &sim_params,
            win,
        ));

        for s in &snaps_sorted {
            summary.snaps_total += 1;
            let Some(input) = snap_to_input(s, tun, ws, we, &overrides) else {
                continue;
            };
            summary.snaps_usable_book += 1;
            let replay = evaluate_manual_signal(&input);
            if let Some(live) = u8_to_label(s.sig) {
                if live != replay {
                    summary.replay_mismatch_live_sig += 1;
                }
            }

            if let Some(pred) = predicted_side_strong(replay) {
                summary.strong_calls += 1;
                if pred == win {
                    summary.strong_correct += 1;
                }
            }

            if mode == EvalMode::WatchAsHint && matches!(replay, ManualSignalLabel::Watch) {
                summary.watch_calls += 1;
                if let Some(pred) = predicted_side_watch_hint(replay, s.spot, s.ptb) {
                    if pred == win {
                        summary.watch_correct += 1;
                    }
                }
            }
        }
    }

    if json_out {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("signal-eval summary ({})", summary.mode);
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
        ingest_jsonl, list_jsonl_files, simulate_trading_round, snap_to_input, RoundAccum,
        SimTradingParams, TunableOverrides,
    };
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

        let paths = list_jsonl_files(&dir, Some("2026-05-11")).unwrap();

        assert_eq!(paths.len(), 2);
        assert!(paths.iter().any(|p| p.ends_with("2026-05-11.jsonl")));
        assert!(paths.iter().any(|p| p.ends_with("2026-05-11-btc-5m.jsonl")));
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
}
