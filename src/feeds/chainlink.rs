//! Polymarket RTDS — Chainlink BTC/USD feed.
//!
//! No authentication required. We subscribe to the `crypto_prices_chainlink`
//! topic with filter `symbol=btc/usd` and emit a `PriceTick` on every update.
//!
//! RTDS expects `action` + a **`subscriptions`** array, and Chainlink filters
//! must be a **JSON string** (e.g. `"{\"symbol\":\"btc/usd\"}"`), not a nested
//! object — see <https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices>.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
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
    /// For "subscribe" envelopes the payload is a batch of points.
    #[serde(default)] data:      Vec<DataPoint>,
}

#[derive(Debug, Deserialize)]
struct DataPoint {
    #[serde(default)] value:     f64,
    #[serde(default)] timestamp: u64,
}

/// Spawns a task that maintains the WS connection with reconnect-on-error,
/// pushing every new price tick into `tx`.
pub fn spawn(tx: mpsc::Sender<PriceTick>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_once(&tx).await {
                warn!(error = %e, "chainlink WS disconnected — reconnecting in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    })
}

async fn run_once(tx: &mpsc::Sender<PriceTick>) -> Result<()> {
    let (mut ws, _) = crate::net::ws_connect(RTDS_WS_URL)
        .await
        .context("connect to Polymarket RTDS")?;

    info!(url = %RTDS_WS_URL, "RTDS (Chainlink) connected");

    // Documented shape: filters is a string containing JSON, inside `subscriptions`.
    let filters = serde_json::to_string(&serde_json::json!({ "symbol": "btc/usd" }))
        .expect("static json");
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

    // Heartbeat — docs: PING every 5s.
    let mut ping_iv = tokio::time::interval(std::time::Duration::from_secs(5));
    ping_iv.tick().await;

    let mut first_text = true;

    loop {
        tokio::select! {
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
                                        || p.symbol.eq_ignore_ascii_case("btc/usd");
                                    if !sym_ok {
                                        continue;
                                    }
                                    if let (Some(v), Some(ts)) = (p.value, p.timestamp) {
                                        let _ = tx.send(PriceTick { price: v, timestamp_ms: ts }).await;
                                    }
                                    if let Some(last) = p.data.last() {
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
