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
use num_bigint::BigInt;
use num_traits::Signed;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

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

fn build_sdk_config() -> Result<Config> {
    let api_key = std::env::var("CHAINLINK_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .context("CHAINLINK_API_KEY missing")?;
    let user_secret = std::env::var("CHAINLINK_USER_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .context("CHAINLINK_USER_SECRET missing")?;
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

    let mut stream = Stream::new(&cfg, vec![feed_id]).await.context("Data Streams Stream::new")?;
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
                        let price = bigint_18decimals_to_f64(&report_data.benchmark_price);
                        let ts_sec = response.report.observations_timestamp as u64;
                        let tick = PriceTick {
                            price,
                            timestamp_ms: ts_sec.saturating_mul(1000),
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

fn bigint_18decimals_to_f64(b: &BigInt) -> f64 {
    // Crypto reference streams use 18 decimals (Chainlink Data Streams docs).
    let neg = b.is_negative();
    let digits = b.abs().to_string();
    if digits == "0" {
        return 0.0;
    }
    let v = if digits.len() <= 18 {
        let pad = 18 - digits.len();
        let frac = format!("{}{}", "0".repeat(pad), digits);
        format!("0.{frac}").parse::<f64>().unwrap_or(0.0)
    } else {
        let split = digits.len() - 18;
        format!("{}.{}" , &digits[..split], &digits[split..])
            .parse::<f64>()
            .unwrap_or(0.0)
    };
    if neg { -v } else { v }
}
