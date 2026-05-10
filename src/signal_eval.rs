//! Offline replay of [`crate::strategy::evaluate_manual_signal`] on JSONL `snap` rows.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::round_log::{u8_to_label, u8_to_sentiment, RoundLogStrategyTunables};
use crate::strategy::{
    evaluate_manual_signal, ManualSignalBookSide, ManualSignalInput, ManualSignalLabel,
};

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
                    .map(|s| s.to_string_lossy() == d)
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

struct TunableOverrides {
    strong_gap_mult: Option<f64>,
    max_spread_mult: Option<f64>,
    min_top_ask_shares: Option<f64>,
    watch_ratio: Option<f64>,
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
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }

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

        for s in &acc.snaps {
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
    }

    Ok(())
}

#[cfg(test)]
mod signal_eval_tests {
    use std::collections::HashMap;
    use std::io::Write;

    use super::{ingest_jsonl, snap_to_input, RoundAccum, TunableOverrides};
    use crate::strategy::{evaluate_manual_signal, ManualSignalLabel};

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
}
