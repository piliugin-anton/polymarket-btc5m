//! Polymarket CLOB WebSocket — public `market` channel for order book + trades.
//!
//! We subscribe to the two token IDs (UP and DOWN) of the active market and
//! consume book snapshots + price changes. The feed may send either tagged
//! `event_type` messages, or a wrapped `{ "market", "price_changes": [...] }`
//! batch with per-row `asset_id`. For user-specific fills we'd need
//! the `user` channel with signed auth — left as a follow-up (see trading.rs
//! for how we'd derive the L2 creds).

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::config::CLOB_WS_URL;

fn snip_frame(txt: &str, max: usize) -> String {
    let t = txt.trim();
    if t.len() <= max {
        t.to_string()
    } else {
        format!("{}…", &t[..max])
    }
}

#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: f64,
    pub size:  f64,
}

#[derive(Debug, Clone)]
pub struct BookSnapshot {
    pub asset_id: String,
    pub bids:     Vec<BookLevel>, // sorted high → low
    pub asks:     Vec<BookLevel>, // sorted low → high
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event_type")]
enum RawEvent {
    #[serde(rename = "book")]
    Book {
        asset_id: String,
        #[serde(default)] bids: Vec<RawLevel>,
        #[serde(default)] asks: Vec<RawLevel>,
    },
    #[serde(rename = "price_change")]
    PriceChange {
        asset_id: String,
        #[serde(default)] changes: Vec<RawChange>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct RawLevel {
    price: String,
    size:  String,
}

#[derive(Debug, Deserialize)]
struct RawChange {
    price: String,
    side:  String, // "BUY" or "SELL"
    size:  String,
}

/// Alternate CLOB shape: one object with `market` + `price_changes` (per-row `asset_id`).
#[derive(Debug, Deserialize)]
struct MarketPriceChangesMsg {
    #[serde(default, rename = "market")]
    #[allow(dead_code)]
    market: Option<String>,
    #[serde(default)]
    price_changes: Vec<MarketPriceChangeItem>,
}

#[derive(Debug, Deserialize)]
struct MarketPriceChangeItem {
    asset_id: String,
    price:    String,
    size:     String,
    side:     String,
}

pub fn spawn(token_ids: Vec<String>, tx: mpsc::Sender<BookSnapshot>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_once(&token_ids, &tx).await {
                warn!(error = %e, "CLOB WS disconnected — reconnecting in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    })
}

async fn run_once(token_ids: &[String], tx: &mpsc::Sender<BookSnapshot>) -> Result<()> {
    let (mut ws, _) = crate::net::ws_connect(CLOB_WS_URL)
        .await
        .context("connect to Polymarket CLOB WS")?;

    info!(url = %CLOB_WS_URL, n = token_ids.len(), "CLOB WS connected");

    // Market channel subscribe message (see Polymarket WSS docs — `type` + `assets_ids`).
    let sub = serde_json::json!({
        "type":          "market",
        "assets_ids":    token_ids,
    });
    ws.send(Message::Text(sub.to_string().into())).await?;
    for (i, id) in token_ids.iter().enumerate() {
        let prefix: String = id.chars().take(12).collect();
        info!(i, token_id_prefix = %prefix, "CLOB WS subscribe token");
    }

    // Keep local book state so price_change events can be applied
    // asset_id → (bids map, asks map). We use i64 keyed by price*100 because
    // price tick is 0.01 and BTreeMap needs Ord.
    let mut books: HashMap<String, (BTreeMap<i64, f64>, BTreeMap<i64, f64>)> = HashMap::new();

    let mut ping = tokio::time::interval(std::time::Duration::from_secs(10));
    ping.tick().await;

    let mut first_text_logged = false;
    let mut first_non_text_logged = false;
    let mut n_frames = 0u64;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                let _ = ws.send(Message::Text("PING".into())).await;
            }
            m = ws.next() => {
                let m = match m {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => {
                        info!(n_frames, "CLOB WS stream ended");
                        return Ok(());
                    }
                };
                let txt = match m {
                    Message::Text(t) => t,
                    Message::Close(f) => {
                        info!(?f, "CLOB WS peer closed");
                        return Ok(());
                    }
                    other => {
                        if !first_non_text_logged {
                            info!(?other, "CLOB WS first non-text frame");
                            first_non_text_logged = true;
                        }
                        continue;
                    }
                };
                if txt.trim() == "PONG" { continue; }

                n_frames += 1;
                if !first_text_logged {
                    info!(
                        len = txt.len(),
                        snippet = %snip_frame(&txt, 240),
                        "CLOB WS first text payload (book / price_change / …)"
                    );
                    first_text_logged = true;
                }

                // CLOB: `[{event_type:...}]`, single event, or `{ market, price_changes:[...] }`.
                if let Ok(events) = serde_json::from_str::<Vec<RawEvent>>(&txt) {
                    for ev in events {
                        apply_raw_event(&mut books, ev, tx).await;
                    }
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<RawEvent>(&txt) {
                    apply_raw_event(&mut books, ev, tx).await;
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<MarketPriceChangesMsg>(&txt) {
                    for ch in &msg.price_changes {
                        let entry = books.entry(ch.asset_id.clone()).or_default();
                        let (Ok(p), Ok(s)) = (ch.price.parse::<f64>(), ch.size.parse::<f64>()) else { continue };
                        let key = (p * 100.0).round() as i64;
                        let map = if ch.side == "BUY" { &mut entry.0 } else { &mut entry.1 };
                        if s == 0.0 { map.remove(&key); } else { map.insert(key, s); }
                        send_snapshot(&ch.asset_id, entry, tx).await;
                    }
                    continue;
                }

                warn!(
                    len = txt.len(),
                    snippet = %snip_frame(&txt, 280),
                    "CLOB WS text not parsed (book / price_change / market price_changes)",
                );
            }
        }
    }
}

async fn apply_raw_event(
    books: &mut HashMap<String, (BTreeMap<i64, f64>, BTreeMap<i64, f64>)>,
    ev: RawEvent,
    tx: &mpsc::Sender<BookSnapshot>,
) {
    match ev {
        RawEvent::Book { asset_id, bids, asks } => {
            let entry = books.entry(asset_id.clone()).or_default();
            entry.0.clear();
            entry.1.clear();
            for l in &bids {
                if let (Ok(p), Ok(s)) = (l.price.parse::<f64>(), l.size.parse::<f64>()) {
                    entry.0.insert((p * 100.0).round() as i64, s);
                }
            }
            for l in &asks {
                if let (Ok(p), Ok(s)) = (l.price.parse::<f64>(), l.size.parse::<f64>()) {
                    entry.1.insert((p * 100.0).round() as i64, s);
                }
            }
            send_snapshot(&asset_id, entry, tx).await;
        }
        RawEvent::PriceChange { asset_id, changes } => {
            let entry = books.entry(asset_id.clone()).or_default();
            for c in &changes {
                let (Ok(p), Ok(s)) = (c.price.parse::<f64>(), c.size.parse::<f64>()) else { continue };
                let key = (p * 100.0).round() as i64;
                let map = if c.side == "BUY" { &mut entry.0 } else { &mut entry.1 };
                if s == 0.0 { map.remove(&key); } else { map.insert(key, s); }
            }
            send_snapshot(&asset_id, entry, tx).await;
        }
        RawEvent::Other => {}
    }
}

async fn send_snapshot(
    asset_id: &str,
    (bids, asks): &(BTreeMap<i64, f64>, BTreeMap<i64, f64>),
    tx: &mpsc::Sender<BookSnapshot>,
) {
    // bids: high→low, asks: low→high
    let bids: Vec<_> = bids.iter().rev().map(|(p, s)| BookLevel { price: *p as f64 / 100.0, size: *s }).collect();
    let asks: Vec<_> = asks.iter().map(|(p, s)| BookLevel { price: *p as f64 / 100.0, size: *s }).collect();
    let _ = tx.send(BookSnapshot { asset_id: asset_id.to_string(), bids, asks }).await;
}
