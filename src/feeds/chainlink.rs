//! Spot crypto/USD price for the TUI: either **Polymarket RTDS** (unauthenticated) or,
//! when `CHAINLINK_API_KEY` and `CHAINLINK_USER_SECRET` are set, **Chainlink Data Streams**
//! over the official Rust SDK (see `chainlink_data_streams.rs`).
//!
//! RTDS: `crypto_prices_chainlink` with `filters: "{\"symbol\":\"eth/usd\"}"`.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::config::RTDS_WS_URL;

fn snip(txt: &str, max: usize) -> String {
    let t = txt.trim();
    if t.len() <= max {
        t.to_string()
    } else {
        format!("{}…", &t[..max])
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PriceTick {
    pub price:        f64,
    pub timestamp_ms: u64,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)] topic: String,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    kind: String,
    #[serde(default)] payload: Option<Payload>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(default)] symbol:    String,
    #[serde(default)] value:     Option<f64>,
    #[serde(default)] timestamp: Option<u64>,
    #[serde(default)] data:      Vec<DataPoint>,
}

#[derive(Debug, Deserialize)]
struct DataPoint {
    #[serde(default)] value:     f64,
    #[serde(default)] timestamp: u64,
}

fn chainlink_data_streams_env_set() -> bool {
    let k = std::env::var("CHAINLINK_API_KEY").ok().filter(|s| !s.trim().is_empty());
    let s = std::env::var("CHAINLINK_USER_SECRET").ok().filter(|s| !s.trim().is_empty());
    k.is_some() && s.is_some()
}

/// Waits for a **non-empty** `symbol` on `symbol_rx` (e.g. `btc/usd`), then connects, reconnects
/// on drop/failure, and re-reads the watch value so symbol changes can take effect.
pub fn spawn(
    tx:         mpsc::Sender<PriceTick>,
    symbol_rx: watch::Receiver<String>,
) -> tokio::task::JoinHandle<()> {
    if chainlink_data_streams_env_set() {
        info!("using Chainlink Data Streams WebSocket (CHAINLINK_API_KEY + CHAINLINK_USER_SECRET)");
        super::chainlink_data_streams::spawn(tx, symbol_rx)
    } else {
        spawn_rtds(tx, symbol_rx)
    }
}

fn spawn_rtds(
    tx:         mpsc::Sender<PriceTick>,
    mut symbol_rx: watch::Receiver<String>,
) -> tokio::task::JoinHandle<()> {
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
            if let Err(e) = run_rtds_once(&tx, &sym, &mut symbol_rx).await {
                warn!(error = %e, sym = %sym, "RTDS (Chainlink) disconnected — retry in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    })
}

async fn run_rtds_once(
    tx: &mpsc::Sender<PriceTick>,
    rtds_symbol: &str,
    symbol_rx: &mut watch::Receiver<String>,
) -> Result<()> {
    let (mut ws, _) = crate::net::ws_connect(RTDS_WS_URL)
        .await
        .context("connect to Polymarket RTDS")?;

    info!(url = %RTDS_WS_URL, %rtds_symbol, "RTDS (Chainlink) connected");

    let filters = serde_json::to_string(&serde_json::json!({ "symbol": rtds_symbol }))
        .expect("static json to string");
    let sub = serde_json::json!({
        "action": "subscribe",
        "subscriptions": [
            {
                "topic": "crypto_prices_chainlink",
                "type": "*",
                "filters": filters,
            }
        ]
    });
    let sub_txt = sub.to_string();
    info!(payload = %snip(&sub_txt, 200), "RTDS subscribe sent");
    ws.send(Message::Text(sub_txt.into())).await?;

    let mut ping_iv = tokio::time::interval(std::time::Duration::from_secs(5));
    ping_iv.tick().await;

    let mut first_text = true;
    let sym_expect = rtds_symbol.to_ascii_lowercase();

    loop {
        tokio::select! {
            biased;
            ch = symbol_rx.changed() => {
                ch.map_err(|_| anyhow::anyhow!("RTDS symbol watch closed"))?;
                if symbol_rx.borrow().as_str() != rtds_symbol {
                    info!(
                        old = %rtds_symbol,
                        new = %symbol_rx.borrow().as_str(),
                        "RTDS resubscribe: symbol changed"
                    );
                    return Ok(());
                }
            }
            _ = ping_iv.tick() => {
                let _ = ws.send(Message::Text("PING".into())).await;
            }
            msg = ws.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => {
                        info!("RTDS stream ended");
                        return Ok(());
                    }
                };
                match msg {
                    Message::Text(txt) => {
                        if txt.trim() == "PONG" { continue; }
                        if first_text {
                            info!(
                                len = txt.len(),
                                snippet = %snip(&txt, 240),
                                "RTDS first text frame"
                            );
                            first_text = false;
                        }
                        match serde_json::from_str::<Envelope>(&txt) {
                            Ok(env) if env.topic == "crypto_prices_chainlink" => {
                                if let Some(p) = env.payload {
                                    let sym_ok = p.symbol.is_empty()
                                        || p.symbol.to_ascii_lowercase() == sym_expect;
                                    if !sym_ok {
                                        continue;
                                    }
                                    if let (Some(v), Some(ts)) = (p.value, p.timestamp) {
                                        let _ = tx.send(PriceTick { price: v, timestamp_ms: ts }).await;
                                    } else if let Some(last) = p.data.last() {
                                        let _ = tx.send(PriceTick {
                                            price: last.value,
                                            timestamp_ms: last.timestamp,
                                        }).await;
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => warn!(
                                %e,
                                raw = %snip(&txt, 400),
                                "RTDS message JSON parse failed"
                            ),
                        }
                    }
                    Message::Close(f) => {
                        info!(?f, "RTDS peer closed");
                        return Ok(());
                    }
                    other => {
                        tracing::debug!(?other, "RTDS non-text frame");
                    }
                }
            }
        }
    }
}
