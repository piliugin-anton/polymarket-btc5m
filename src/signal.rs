//! Live signal composition: rubric ([`crate::strategy::evaluate_manual_signal`]) plus optional
//! per-market JSON tunables under [`POLYMARKET_SIGNAL_MODEL_DIR`](crate::config::Config) named
//! `{crypto}_{time_window}.json` (e.g. `btc_5m.json`). When such a file exists, parses, and
//! supplies at least one tunable override, a **model** label is computed by re-running the rubric
//! on a merged [`ManualSignalInput`]. [`SignalBundle::effective`] prefers the model label when
//! present (see product wiring in `AppState` / autotrading).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use serde::Deserialize;

use crate::market_profile::{MarketProfile, Timeframe};
use crate::strategy::{evaluate_manual_signal, ManualSignalInput, ManualSignalLabel};

/// `POLYMARKET_SIGNAL_MODEL_DIR` / `{model_id}.json` — e.g. `btc_5m`, `eth_15m`, `sol_1d`.
pub fn model_id(profile: &MarketProfile) -> String {
    let tf = match profile.timeframe {
        Timeframe::M5 => "5m",
        Timeframe::M15 => "15m",
        Timeframe::D1 => "1d",
    };
    format!("{}_{}", profile.asset.rolling_slug_prefix, tf)
}

#[derive(Debug, Clone, Copy)]
pub struct SignalBundle {
    pub rubric: ManualSignalLabel,
    /// Present when a model file exists, parses, and supplies at least one tunable override.
    pub model: Option<ManualSignalLabel>,
}

impl SignalBundle {
    #[must_use]
    pub fn effective(self) -> ManualSignalLabel {
        self.model.unwrap_or(self.rubric)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ModelFile {
    #[serde(default, alias = "version")]
    _version: u32,
    strong_gap_mult: Option<f64>,
    max_spread_mult: Option<f64>,
    min_top_ask_shares: Option<f64>,
    watch_ratio: Option<f64>,
}

fn merge_input(input: &ManualSignalInput, mf: &ModelFile) -> ManualSignalInput {
    ManualSignalInput {
        spot_price: input.spot_price,
        price_to_beat: input.price_to_beat,
        up: input.up,
        down: input.down,
        seconds_to_close: input.seconds_to_close,
        window_secs: input.window_secs,
        sentiment: input.sentiment,
        activity_notional_60s: input.activity_notional_60s,
        strong_gap_mult: mf
            .strong_gap_mult
            .unwrap_or(input.strong_gap_mult),
        max_spread_mult: mf.max_spread_mult.unwrap_or(input.max_spread_mult),
        min_top_ask_shares: mf
            .min_top_ask_shares
            .unwrap_or(input.min_top_ask_shares),
        watch_ratio: mf.watch_ratio.unwrap_or(input.watch_ratio),
    }
}

fn model_file_has_override(mf: &ModelFile) -> bool {
    mf.strong_gap_mult.is_some()
        || mf.max_spread_mult.is_some()
        || mf.min_top_ask_shares.is_some()
        || mf.watch_ratio.is_some()
}

type CacheKey = String;
type CacheVal = (SystemTime, Option<ModelFile>);

static MODEL_JSON_CACHE: LazyLock<Mutex<HashMap<CacheKey, CacheVal>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn load_model_file(dir: &Path, profile: &MarketProfile) -> Option<ModelFile> {
    let id = model_id(profile);
    let path = dir.join(format!("{id}.json"));
    let key = path.to_string_lossy().to_string();
    let meta = std::fs::metadata(&path).ok()?;
    let mtime = meta.modified().ok()?;
    let mut guard = MODEL_JSON_CACHE.lock().expect("signal model cache");
    if let Some((t, parsed)) = guard.get(&key) {
        if *t == mtime {
            return parsed.clone();
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let parsed: ModelFile = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(path = %key, err = %e, "signal model JSON parse failed");
            guard.insert(key, (mtime, None));
            return None;
        }
    };
    if !model_file_has_override(&parsed) {
        guard.insert(key, (mtime, None));
        return None;
    }
    guard.insert(key, (mtime, Some(parsed.clone())));
    Some(parsed)
}

/// `model_dir`: from [`crate::config::Config::signal_model_dir`]. `profile`: active market profile
/// when known (after wizard / headless start).
pub fn evaluate(
    model_dir: &Path,
    profile: Option<&MarketProfile>,
    input: &ManualSignalInput,
) -> SignalBundle {
    let rubric = evaluate_manual_signal(input);
    let model = profile.and_then(|p| {
        let mf = load_model_file(model_dir, p)?;
        let merged = merge_input(input, &mf);
        let mlabel = evaluate_manual_signal(&merged);
        Some(mlabel)
    });
    SignalBundle { rubric, model }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_profile::MarketProfile;
    use crate::strategy::{
        ManualSignalBookSide, ManualSignalInput, ManualSignalSentiment,
    };

    fn btc_5m_profile() -> MarketProfile {
        MarketProfile::parse_cli_token("btc-5m").unwrap()
    }

    #[test]
    fn model_id_btc_5m() {
        assert_eq!(model_id(&btc_5m_profile()), "btc_5m");
    }

    #[test]
    fn model_id_eth_15m_case_insensitive_profile() {
        let p = MarketProfile::parse_cli_token("ETH-15M").unwrap();
        assert_eq!(model_id(&p), "eth_15m");
    }

    #[test]
    fn effective_falls_back_when_no_file() {
        let dir = std::env::temp_dir().join(format!("signal_models_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = ManualSignalInput {
            spot_price: Some(100.5),
            price_to_beat: Some(100.0),
            up: ManualSignalBookSide {
                best_bid: Some(0.4),
                best_ask: Some(0.41),
                best_ask_size: Some(10.0),
            },
            down: ManualSignalBookSide {
                best_bid: Some(0.58),
                best_ask: Some(0.59),
                best_ask_size: Some(10.0),
            },
            seconds_to_close: Some(200),
            window_secs: Some(300),
            sentiment: ManualSignalSentiment::Neutral,
            activity_notional_60s: 50.0,
            strong_gap_mult: 1.0,
            max_spread_mult: 1.0,
            min_top_ask_shares: 5.0,
            watch_ratio: 0.6,
        };
        let b = evaluate(&dir, Some(&btc_5m_profile()), &input);
        assert!(b.model.is_none());
        assert_eq!(b.effective(), b.rubric);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_file_can_change_label() {
        let dir = std::env::temp_dir().join(format!("signal_models_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("btc_5m.json"),
            r#"{"version":1,"min_top_ask_shares":500.0}"#,
        )
        .unwrap();
        let input = ManualSignalInput {
            spot_price: Some(100.5),
            price_to_beat: Some(100.0),
            up: ManualSignalBookSide {
                best_bid: Some(0.4),
                best_ask: Some(0.41),
                best_ask_size: Some(10.0),
            },
            down: ManualSignalBookSide {
                best_bid: Some(0.58),
                best_ask: Some(0.59),
                best_ask_size: Some(10.0),
            },
            seconds_to_close: Some(200),
            window_secs: Some(300),
            sentiment: ManualSignalSentiment::Neutral,
            activity_notional_60s: 50.0,
            strong_gap_mult: 1.0,
            max_spread_mult: 1.0,
            min_top_ask_shares: 5.0,
            watch_ratio: 0.6,
        };
        let b = evaluate(&dir, Some(&btc_5m_profile()), &input);
        assert!(b.model.is_some());
        assert_eq!(b.model, Some(ManualSignalLabel::NoTrade));
        assert_ne!(b.effective(), b.rubric, "model should thin book below min ask");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
