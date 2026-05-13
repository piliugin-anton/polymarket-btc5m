//! Append-only JSONL session log for round context (open / snap / close) and optional fills.
//! Schema version `v: 1`, line discriminator `t`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::app::AppState;
use crate::gamma::ActiveMarket;
use crate::market_profile::MarketProfile;
use crate::strategy::{ManualSignalLabel, ManualSignalSentiment};
use crate::trading::Side;

const SCHEMA_V: u32 = 1;

/// Tunables persisted on each `open` line for offline replay.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoundLogStrategyTunables {
    pub strong_gap_mult: f64,
    pub max_spread_mult: f64,
    pub min_top_ask_shares: f64,
    pub watch_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
struct CloseLine {
    t: &'static str,
    v: u32,
    cid: String,
    ts_close: i64,
    spot_last: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    px: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    win: Option<&'static str>,
    src: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct OpenLine {
    t: &'static str,
    v: u32,
    cid: String,
    slug: String,
    ws: i64,
    we: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ptb: Option<f64>,
    up: String,
    down: String,
    asset: String,
    #[serde(flatten)]
    tunables: RoundLogStrategyTunables,
}

#[derive(Debug, Clone, Serialize)]
struct SnapLine {
    t: &'static str,
    v: u32,
    cid: String,
    ts: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    spot: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ptb: Option<f64>,
    sig: u8,
    sent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    ubu: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uba: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dbu: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dba: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ubas: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dbas: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secs: Option<i64>,
    act: f64,
}

#[derive(Debug, Clone, Serialize)]
struct FillLine {
    t: &'static str,
    v: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cid: Option<String>,
    ts: i64,
    side: &'static str,
    oc: &'static str,
    qty: f64,
    px: f64,
    pnl: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tid: Option<String>,
}

fn label_to_u8(l: ManualSignalLabel) -> u8 {
    match l {
        ManualSignalLabel::NoTrade => 0,
        ManualSignalLabel::Watch => 1,
        ManualSignalLabel::StrongUp => 2,
        ManualSignalLabel::StrongDown => 3,
    }
}

fn sentiment_to_u8(s: ManualSignalSentiment) -> u8 {
    match s {
        ManualSignalSentiment::Up => 0,
        ManualSignalSentiment::Down => 1,
        ManualSignalSentiment::Neutral => 2,
        ManualSignalSentiment::Unknown => 3,
    }
}

pub fn u8_to_label(v: u8) -> Option<ManualSignalLabel> {
    match v {
        0 => Some(ManualSignalLabel::NoTrade),
        1 => Some(ManualSignalLabel::Watch),
        2 => Some(ManualSignalLabel::StrongUp),
        3 => Some(ManualSignalLabel::StrongDown),
        _ => None,
    }
}

pub fn u8_to_sentiment(v: u8) -> Option<ManualSignalSentiment> {
    match v {
        0 => Some(ManualSignalSentiment::Up),
        1 => Some(ManualSignalSentiment::Down),
        2 => Some(ManualSignalSentiment::Neutral),
        3 => Some(ManualSignalSentiment::Unknown),
        _ => None,
    }
}

/// Approximate winner from spot vs price-to-beat (not official Polymarket resolution).
pub fn approx_win_from_spot(spot: f64, ptb: f64) -> Option<&'static str> {
    if !spot.is_finite() || !ptb.is_finite() {
        return None;
    }
    if spot > ptb {
        Some("up")
    } else if spot < ptb {
        Some("down")
    } else {
        None
    }
}

fn round_log_path_for_date(dir: &Path, day: NaiveDate, profile: Option<(&str, &str)>) -> PathBuf {
    match profile {
        Some((asset, timeframe)) => dir.join(format!(
            "{}-{}-{}.jsonl",
            day,
            sanitize_round_log_path_component(asset),
            sanitize_round_log_path_component(timeframe)
        )),
        None => dir.join(format!("{}.jsonl", day)),
    }
}

/// Lowercase path segment for round-log filenames (matches [`round_log_path_for_date`]).
pub fn sanitize_round_log_path_component(value: &str) -> String {
    let s: String = value
        .trim()
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c == '-' || c == '_' {
                Some(c)
            } else if c.is_ascii_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect();
    if s.is_empty() {
        "unknown".to_string()
    } else {
        s
    }
}

/// `(asset_segment, timeframe_segment)` as used in `{day}-{asset}-{tf}.jsonl`.
pub fn round_log_path_suffix_for_profile(profile: &MarketProfile) -> (String, String) {
    (
        sanitize_round_log_path_component(profile.asset.label),
        sanitize_round_log_path_component(profile.timeframe.label()),
    )
}

/// Expected JSONL stem for a calendar `day` (`YYYY-MM-DD`) and market profile.
pub fn expected_round_log_stem(day: &str, profile: &MarketProfile) -> String {
    let (a, t) = round_log_path_suffix_for_profile(profile);
    format!("{day}-{a}-{t}")
}

#[derive(Debug, Clone)]
pub struct RoundLogWriterConfig {
    pub dir: PathBuf,
    pub snap_interval: Duration,
    pub log_fills: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoundLogFileContext {
    asset: String,
    timeframe: String,
}

enum WriterMsg {
    Line(String),
    SetFileContext(RoundLogFileContext),
    Shutdown,
}

/// Handle enqueueing log lines; writer task runs in background.
#[derive(Clone)]
pub struct RoundLogHandle {
    tx: mpsc::Sender<WriterMsg>,
    snap_interval: Duration,
    log_fills: bool,
    last_snap_at: Arc<Mutex<Option<Instant>>>,
}

impl std::fmt::Debug for RoundLogHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoundLogHandle")
            .field("snap_interval", &self.snap_interval)
            .field("log_fills", &self.log_fills)
            .finish_non_exhaustive()
    }
}

impl RoundLogHandle {
    pub fn spawn(cfg: RoundLogWriterConfig) -> Self {
        let (tx, mut rx) = mpsc::channel::<WriterMsg>(4096);
        let dir = cfg.dir.clone();
        tokio::spawn(async move {
            let mut current_path: Option<PathBuf> = None;
            let mut file_context: Option<RoundLogFileContext> = None;
            let mut file: Option<tokio::fs::File> = None;
            while let Some(msg) = rx.recv().await {
                match msg {
                    WriterMsg::Shutdown => break,
                    WriterMsg::SetFileContext(ctx) => {
                        if file_context.as_ref() != Some(&ctx) {
                            file_context = Some(ctx);
                            current_path = None;
                            file = None;
                        }
                    }
                    WriterMsg::Line(s) => {
                        let today = Utc::now().date_naive();
                        let path = round_log_path_for_date(
                            &dir,
                            today,
                            file_context
                                .as_ref()
                                .map(|ctx| (ctx.asset.as_str(), ctx.timeframe.as_str())),
                        );
                        if current_path.as_ref() != Some(&path) {
                            if let Some(parent) = path.parent() {
                                let _ = tokio::fs::create_dir_all(parent).await;
                            }
                            match tokio::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&path)
                                .await
                            {
                                Ok(f) => {
                                    tracing::debug!(path = %path.display(), "round log opened");
                                    current_path = Some(path);
                                    file = Some(f);
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, path = %path.display(), "round log open failed");
                                    current_path = None;
                                    file = None;
                                }
                            }
                        }
                        if let Some(ref mut f) = file {
                            if let Err(e) = f.write_all(s.as_bytes()).await {
                                tracing::warn!(error = %e, "round log write failed");
                            } else if let Err(e) = f.write_all(b"\n").await {
                                tracing::warn!(error = %e, "round log newline failed");
                            }
                        }
                    }
                }
            }
        });

        Self {
            tx,
            snap_interval: cfg.snap_interval,
            log_fills: cfg.log_fills,
            last_snap_at: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_market_profile(&self, profile: &MarketProfile) {
        let ctx = RoundLogFileContext {
            asset: profile.asset.label.to_string(),
            timeframe: profile.timeframe.label().to_string(),
        };
        match self.tx.try_send(WriterMsg::SetFileContext(ctx)) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("round log channel full — dropping file context update");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    fn try_send_line(&self, json: &impl Serialize) {
        let Ok(bytes) = serde_json::to_string(json) else {
            tracing::warn!("round log: serde failed");
            return;
        };
        match self.tx.try_send(WriterMsg::Line(bytes)) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("round log channel full — dropping line");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// After a successful market roll: optionally finalize `prev`, then `open` for `new_market`.
    pub fn on_market_roll(
        &self,
        prev_market: Option<&ActiveMarket>,
        new_market: &ActiveMarket,
        spot_last_before_roll: Option<f64>,
        state: &AppState,
        profile: Option<&MarketProfile>,
    ) {
        let now = Utc::now().timestamp();
        if let Some(prev) = prev_market {
            let (win, src) = match (spot_last_before_roll, prev.price_to_beat) {
                (Some(spot), Some(ptb)) => match approx_win_from_spot(spot, ptb) {
                    Some(w) => (Some(w), "approx_spot"),
                    None => (None, "approx_spot"),
                },
                _ => (None, "approx_spot"),
            };
            let line = CloseLine {
                t: "close",
                v: SCHEMA_V,
                cid: prev.condition_id.clone(),
                ts_close: now,
                spot_last: spot_last_before_roll,
                px: None,
                win,
                src,
            };
            self.try_send_line(&line);
        }

        let asset = profile
            .map(|p| p.asset.label.to_string())
            .unwrap_or_else(|| "unknown".into());
        let tunables = RoundLogStrategyTunables {
            strong_gap_mult: state.strategy_strong_gap_mult,
            max_spread_mult: state.strategy_max_spread_mult,
            min_top_ask_shares: state.strategy_min_top_ask_shares,
            watch_ratio: state.strategy_watch_ratio,
        };
        let open = OpenLine {
            t: "open",
            v: SCHEMA_V,
            cid: new_market.condition_id.clone(),
            slug: new_market.slug.clone(),
            ws: new_market.opens_at.timestamp(),
            we: new_market.closes_at.timestamp(),
            ptb: new_market.price_to_beat,
            up: new_market.up_token_id.clone(),
            down: new_market.down_token_id.clone(),
            asset,
            tunables,
        };
        self.try_send_line(&open);
    }

    /// Best-effort incomplete close on shutdown (no reliable `win`).
    pub fn shutdown_incomplete(&self, market: &ActiveMarket, spot_last: Option<f64>) {
        let line = CloseLine {
            t: "close",
            v: SCHEMA_V,
            cid: market.condition_id.clone(),
            ts_close: Utc::now().timestamp(),
            spot_last,
            px: None,
            win: None,
            src: "incomplete",
        };
        self.try_send_line(&line);
    }

    pub fn maybe_snap(&self, state: &AppState) {
        let Some(m) = &state.market else {
            return;
        };
        let mut last = self.last_snap_at.lock().expect("mutex");
        let now = Instant::now();
        if let Some(prev) = *last {
            if now.duration_since(prev) < self.snap_interval {
                return;
            }
        }
        *last = Some(now);
        drop(last);

        let snap = state.manual_signal_input_snapshot();
        let sig = label_to_u8(state.manual_signal_label());
        let sent = sentiment_to_u8(snap.sentiment);
        let line = SnapLine {
            t: "snap",
            v: SCHEMA_V,
            cid: m.condition_id.clone(),
            ts: Utc::now().timestamp(),
            spot: snap.spot_price,
            ptb: snap.price_to_beat,
            sig,
            sent,
            ubu: snap.up.best_bid,
            uba: snap.up.best_ask,
            dbu: snap.down.best_bid,
            dba: snap.down.best_ask,
            ubas: snap.up.best_ask_size,
            dbas: snap.down.best_ask_size,
            secs: snap.seconds_to_close,
            act: snap.activity_notional_60s,
        };
        self.try_send_line(&line);
    }

    pub fn log_fills_enabled(&self) -> bool {
        self.log_fills
    }

    pub fn log_fill(
        &self,
        cid: Option<&str>,
        ts: DateTime<Utc>,
        side: Side,
        outcome: crate::app::Outcome,
        qty: f64,
        px: f64,
        pnl: f64,
        tid: Option<String>,
    ) {
        if !self.log_fills {
            return;
        }
        let side_s = match side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        let oc = match outcome {
            crate::app::Outcome::Up => "up",
            crate::app::Outcome::Down => "down",
        };
        let line = FillLine {
            t: "fill",
            v: SCHEMA_V,
            cid: cid.map(String::from),
            ts: ts.timestamp(),
            side: side_s,
            oc,
            qty,
            px,
            pnl,
            tid,
        };
        self.try_send_line(&line);
    }

    pub fn request_shutdown(&self) {
        let _ = self.tx.try_send(WriterMsg::Shutdown);
    }
}

/// Run `round-log-inspect` CLI: args after subcommand name.
pub fn run_inspect_cli(args: &[String]) -> Result<()> {
    let mut dir = PathBuf::from("./data/rounds");
    let mut day: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    anyhow::bail!("--dir requires a path");
                };
                dir = PathBuf::from(p);
            }
            "--day" => {
                i += 1;
                let Some(d) = args.get(i) else {
                    anyhow::bail!("--day requires YYYY-MM-DD");
                };
                day = Some(d.clone());
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    let paths = list_jsonl_files(&dir, day.as_deref())?;
    if paths.is_empty() {
        println!("No .jsonl files under {}", dir.display());
        return Ok(());
    }

    let mut total_lines = 0u64;
    let mut bad_lines = 0u64;
    let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut snaps_per_cid: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    let mut sig_hist = [0u64; 4];
    let mut sent_hist = [0u64; 4];
    let mut rounds_missing_book = std::collections::HashSet::<String>::new();
    let mut incomplete_closes = 0u64;

    for path in &paths {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        for (lineno, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            total_lines += 1;
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("{}:{}: JSON error: {e}", path.display(), lineno + 1);
                    bad_lines += 1;
                    continue;
                }
            };
            let Some(t) = v.get("t").and_then(|x| x.as_str()) else {
                eprintln!("{}:{}: missing t", path.display(), lineno + 1);
                bad_lines += 1;
                continue;
            };
            *counts.entry(t.to_string()).or_insert(0) += 1;
            match t {
                "snap" => {
                    if let Some(cid) = v.get("cid").and_then(|c| c.as_str()) {
                        *snaps_per_cid.entry(cid.to_string()).or_insert(0) += 1;
                    }
                    if let Some(sig) = v.get("sig").and_then(|x| x.as_u64()) {
                        if sig < 4 {
                            sig_hist[sig as usize] += 1;
                        }
                    }
                    if let Some(sent) = v.get("sent").and_then(|x| x.as_u64()) {
                        if sent < 4 {
                            sent_hist[sent as usize] += 1;
                        }
                    }
                    let book_ok = v.get("ubu").is_some()
                        && v.get("uba").is_some()
                        && v.get("dbu").is_some()
                        && v.get("dba").is_some()
                        && v.get("ubas").is_some()
                        && v.get("dbas").is_some();
                    if !book_ok {
                        if let Some(cid) = v.get("cid").and_then(|c| c.as_str()) {
                            rounds_missing_book.insert(cid.to_string());
                        }
                    }
                }
                "close" => {
                    if v.get("src").and_then(|s| s.as_str()) == Some("incomplete") {
                        incomplete_closes += 1;
                    }
                }
                _ => {}
            }
        }
    }

    println!("Files: {}", paths.len());
    for p in &paths {
        println!("  {}", p.display());
    }
    println!("Total non-empty lines: {total_lines}");
    println!("Malformed lines: {bad_lines}");
    println!("Record counts by t:");
    for (k, c) in &counts {
        println!("  {k}: {c}");
    }
    println!("Snaps per cid ({} rounds):", snaps_per_cid.len());
    for (cid, n) in snaps_per_cid.iter().take(20) {
        println!("  {cid}… : {n} snaps");
    }
    if snaps_per_cid.len() > 20 {
        println!("  …");
    }
    println!("sig histogram [NoTrade, Watch, StrongUp, StrongDown]: {sig_hist:?}");
    println!("sent histogram [Up, Down, Neutral, Unknown]: {sent_hist:?}");
    println!(
        "Rounds with any snap missing full book (ubu/uba/dbu/dba/ubas/dbas): {}",
        rounds_missing_book.len()
    );
    println!("close lines with src=incomplete: {incomplete_closes}");

    if bad_lines > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn list_jsonl_files(dir: &Path, day: Option<&str>) -> Result<Vec<PathBuf>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_win_basic() {
        assert_eq!(approx_win_from_spot(101.0, 100.0), Some("up"));
        assert_eq!(approx_win_from_spot(99.0, 100.0), Some("down"));
        assert_eq!(approx_win_from_spot(100.0, 100.0), None);
    }

    #[test]
    fn serde_open_roundtrip() {
        let o = OpenLine {
            t: "open",
            v: 1,
            cid: "c1".into(),
            slug: "s".into(),
            ws: 1,
            we: 2,
            ptb: Some(3.0),
            up: "u".into(),
            down: "d".into(),
            asset: "BTC".into(),
            tunables: RoundLogStrategyTunables {
                strong_gap_mult: 1.0,
                max_spread_mult: 1.0,
                min_top_ask_shares: 5.0,
                watch_ratio: 0.6,
            },
        };
        let s = serde_json::to_string(&o).unwrap();
        assert!(s.contains("\"t\":\"open\""));
        assert!(s.contains("\"v\":1"));
    }

    #[test]
    fn round_log_path_includes_selected_asset_and_timeframe() {
        let dir = PathBuf::from("./data/rounds");
        let day = NaiveDate::from_ymd_opt(2026, 5, 11).unwrap();

        let path = round_log_path_for_date(&dir, day, Some(("BTC", "5m")));

        assert_eq!(path, dir.join("2026-05-11-btc-5m.jsonl"));
    }

    #[test]
    fn day_filter_includes_profile_suffixed_jsonl_files() {
        let dir = std::env::temp_dir().join(format!("round_log_day_filter_{}", std::process::id()));
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
}
