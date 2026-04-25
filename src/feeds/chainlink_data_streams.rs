//! Chainlink Data Streams — authenticated WebSocket via
//! [`chainlink_data_streams_sdk`](https://docs.chain.link/data-streams/reference/data-streams-api/rust-sdk).
//!
//! Activated when `CHAINLINK_API_KEY` and `CHAINLINK_USER_SECRET` are both non-empty (see `.env.example`).
//! Optional: `CHAINLINK_DATA_STREAMS_FEED_ID` overrides the feed derived from the selected asset.

use anyhow::{bail, Context, Result};
use chainlink_data_streams_report::feed_id::ID;
use chainlink_data_streams_report::report::{decode_full_report, v3::ReportDataV3};
use chainlink_data_streams_sdk::config::Config;
use chainlink_data_streams_sdk::stream::Stream;
use num_traits::ToPrimitive;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use uuid::Uuid;

use super::chainlink::PriceTick;

const DEFAULT_REST: &str = "https://api.dataengine.chain.link";
const DEFAULT_WS: &str = "wss://ws.dataengine.chain.link";

fn snip(txt: &str, max: usize) -> String {
    let t = txt.trim();
    if t.len() <= max {
        t.to_string()
    } else {
        format!("{}…", &t[..max])
    }
}

fn resolve_feed_id(rtds_symbol: &str) -> Result<ID> {
    let hex = std::env::var("CHAINLINK_DATA_STREAMS_FEED_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            crate::market_profile::data_streams_feed_id_for_rtds_symbol(rtds_symbol)
                .unwrap_or("")
                .to_string()
        });
    if hex.is_empty() {
        bail!(
            "no Data Streams feed id for symbol {rtds_symbol:?}; set CHAINLINK_DATA_STREAMS_FEED_ID or pick a supported asset"
        );
    }
    ID::from_hex_str(hex.trim()).with_context(|| format!("invalid feed id hex: {}", snip(&hex, 20)))
}

/// Data Engine expects `Authorization` = client UUID and HMAC `user_secret` = longer secret.
/// Env files often swap the two; that produces HTTP 400 on the WS handshake (runtime evidence).
fn normalize_chainlink_credentials(raw_key: &str, raw_secret: &str) -> (String, String, bool) {
    let k = raw_key.trim();
    let s = raw_secret.trim();
    let key_is_uuid = Uuid::parse_str(k).is_ok();
    let secret_is_uuid = Uuid::parse_str(s).is_ok();
    if !key_is_uuid && secret_is_uuid {
        (s.to_string(), k.to_string(), true)
    } else {
        (k.to_string(), s.to_string(), false)
    }
}

fn build_sdk_config() -> Result<Config> {
    let api_key_raw = std::env::var("CHAINLINK_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .context("CHAINLINK_API_KEY missing")?;
    let user_secret_raw = std::env::var("CHAINLINK_USER_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .context("CHAINLINK_USER_SECRET missing")?;
    let (api_key, user_secret, swapped) =
        normalize_chainlink_credentials(&api_key_raw, &user_secret_raw);
    if swapped {
        warn!(
            "CHAINLINK_API_KEY did not parse as UUID but CHAINLINK_USER_SECRET did — using swapped order (API key must be the Data Engine client UUID)"
        );
    }
    let rest = std::env::var("CHAINLINK_DATA_ENGINE_REST_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REST.to_string());
    let ws = std::env::var("CHAINLINK_DATA_ENGINE_WS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WS.to_string());
    Config::new(api_key, user_secret, rest, ws)
        .build()
        .context("Chainlink Data Streams SDK config")
}

/// Same outer loop as RTDS: wait for symbol, connect, reconnect on failure or symbol change.
pub fn spawn(tx: mpsc::Sender<PriceTick>, mut symbol_rx: watch::Receiver<String>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if symbol_rx.borrow().is_empty() {
                if symbol_rx.changed().await.is_err() {
                    return;
                }
                continue;
            }
            let sym = symbol_rx.borrow().clone();
            if sym.is_empty() {
                continue;
            }
            if let Err(e) = run_once(&tx, &sym, &mut symbol_rx).await {
                warn!(error = %e, sym = %sym, "Chainlink Data Streams disconnected — retry in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    })
}

async fn run_once(
    tx: &mpsc::Sender<PriceTick>,
    rtds_symbol: &str,
    symbol_rx: &mut watch::Receiver<String>,
) -> Result<()> {
    let feed_id = resolve_feed_id(rtds_symbol)?;
    let cfg = build_sdk_config()?;

    info!(
        rest = %std::env::var("CHAINLINK_DATA_ENGINE_REST_URL").unwrap_or_else(|_| DEFAULT_REST.into()),
        ws = %std::env::var("CHAINLINK_DATA_ENGINE_WS_URL").unwrap_or_else(|_| DEFAULT_WS.into()),
        %rtds_symbol,
        feed = %feed_id.to_hex_string(),
        "Chainlink Data Streams connecting"
    );

    let mut stream = Stream::new(&cfg, vec![feed_id])
        .await
        .context("Data Streams Stream::new")?;
    stream.listen().await.context("Data Streams listen (WS handshake)")?;

    loop {
        tokio::select! {
            biased;
            ch = symbol_rx.changed() => {
                ch.map_err(|_| anyhow::anyhow!("symbol watch closed"))?;
                if symbol_rx.borrow().as_str() != rtds_symbol {
                    info!(
                        old = %rtds_symbol,
                        new = %symbol_rx.borrow().as_str(),
                        "Data Streams: symbol changed — reconnect"
                    );
                    let _ = stream.close().await;
                    return Ok(());
                }
            }
            read = stream.read() => {
                match read {
                    Ok(response) => {
                        let raw = response.report.full_report.trim();
                        let hex_body = raw.strip_prefix("0x").unwrap_or(raw);
                        let full_report = match hex::decode(hex_body) {
                            Ok(b) => b,
                            Err(e) => {
                                warn!(error = %e, snippet = %snip(raw, 80), "Data Streams full_report hex decode failed");
                                continue;
                            }
                        };
                        let (_ctx, report_blob) = match decode_full_report(&full_report) {
                            Ok(x) => x,
                            Err(e) => {
                                warn!(error = %e, "Data Streams decode_full_report failed");
                                continue;
                            }
                        };
                        let report_data = match ReportDataV3::decode(&report_blob) {
                            Ok(d) => d,
                            Err(e) => {
                                warn!(error = %e, "Data Streams ReportDataV3::decode failed (feed may use a newer report version)");
                                continue;
                            }
                        };
                        // 18-decimal fixed point: i192 → f64 via `num_bigint` (no `to_string` / parse).
                        const SCALE: f64 = 1e-18;
                        let v = report_data.benchmark_price.to_f64().unwrap() * SCALE;
                        let price = if v.is_finite() { v } else { 0.0 };
                        let ts_sec = response.report.observations_timestamp as u64;
                        let tick = PriceTick {
                            price,
                            // Observations are seconds; ms fits easily in u64.
                            timestamp_ms: ts_sec * 1000,
                        };
                        let _ = tx.send(tick).await;
                    }
                    Err(e) => {
                        let _ = stream.close().await;
                        return Err(e).context("Data Streams read");
                    }
                }
            }
        }
    }
}
