//! Polymarket CLOB WebSocket — public `market` channel for order book + trades.
//!
//! We subscribe to the two token IDs (UP and DOWN) of the active market and
//! consume book snapshots + price changes. The feed may send either tagged
//! `event_type` messages, or a wrapped `{ "market", "price_changes": [...] }`
//! batch with per-row `asset_id`. User fills / orders use [`super::clob_user_ws`].

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::config::CLOB_WS_URL;
use crate::trading::canonical_clob_token_id;

/// Book map key: price quantized to 1e-6 (supports ticks finer than 0.01; `(p*100).round()` collided).
const PRICE_KEY_SCALE: f64 = 1_000_000.0;

#[inline]
fn price_to_key(p: f64) -> i64 {
    (p * PRICE_KEY_SCALE).round() as i64
}

#[inline]
fn key_to_price(k: i64) -> f64 {
    k as f64 / PRICE_KEY_SCALE
}

fn snip_frame(txt: &str, max: usize) -> String {
    let t = txt.trim();
    if t.len() <= max {
        t.to_string()
    } else {
        format!("{}…", &t[..max])
    }
}

fn warn_unparsed_clob(txt: &str) {
    warn!(
        len = txt.len(),
        snippet = %snip_frame(txt, 280),
        "CLOB WS text not parsed (book / price_change / market price_changes)",
    );
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
    // asset_id → (bids map, asks map). Keys are micro-ticks (`price_to_key`).
    let mut books: HashMap<String, (BTreeMap<i64, f64>, BTreeMap<i64, f64>)> = HashMap::new();
    // Reuse allocation across snapshots (CLOB can burst price_change updates).
    let mut out_bids: Vec<BookLevel> = Vec::new();
    let mut out_asks: Vec<BookLevel> = Vec::new();

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
                // Heuristic branch avoids 2–3 `from_str` attempts on every frame.
                let t = txt.as_str();
                if t.trim_start().starts_with('[') {
                    if let Ok(events) = serde_json::from_str::<Vec<RawEvent>>(t) {
                        let mut touched: HashSet<String> = HashSet::new();
                        for ev in events {
                            if let Some(aid) = apply_raw_event_to_books(&mut books, ev) {
                                touched.insert(aid);
                            }
                        }
                        for aid in &touched {
                            if let Some(entry) = books.get(aid) {
                                send_snapshot(aid, entry, &mut out_bids, &mut out_asks, tx).await;
                            }
                        }
                    } else {
                        warn_unparsed_clob(&txt);
                    }
                    continue;
                }
                if t.contains("price_changes") && t.trim_start().starts_with('{') {
                    if let Ok(msg) = serde_json::from_str::<MarketPriceChangesMsg>(t) {
                        let mut touched: HashSet<String> = HashSet::new();
                        for ch in &msg.price_changes {
                            let aid = canonical_clob_token_id(&ch.asset_id).into_owned();
                            let entry = books.entry(aid.clone()).or_default();
                            let (Ok(p), Ok(s)) = (ch.price.parse::<f64>(), ch.size.parse::<f64>()) else {
                                continue;
                            };
                            let key = price_to_key(p);
                            let map = if ch.side == "BUY" { &mut entry.0 } else { &mut entry.1 };
                            if s == 0.0 {
                                map.remove(&key);
                            } else {
                                map.insert(key, s);
                            }
                            touched.insert(aid);
                        }
                        for aid in &touched {
                            if let Some(entry) = books.get(aid) {
                                send_snapshot(aid, entry, &mut out_bids, &mut out_asks, tx).await;
                            }
                        }
                    } else {
                        warn_unparsed_clob(t);
                    }
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<RawEvent>(t) {
                    if let Some(aid) = apply_raw_event_to_books(&mut books, ev) {
                        if let Some(entry) = books.get(&aid) {
                            send_snapshot(&aid, entry, &mut out_bids, &mut out_asks, tx).await;
                        }
                    }
                    continue;
                }

                warn_unparsed_clob(&txt);
            }
        }
    }
}

/// Apply a raw event to the local book maps. Returns the canonical asset ID if the
/// event touched a book (i.e., `Book` or `PriceChange`); `None` for `Other`.
/// Does NOT send a snapshot — callers batch and send after the whole message.
fn apply_raw_event_to_books(
    books: &mut HashMap<String, (BTreeMap<i64, f64>, BTreeMap<i64, f64>)>,
    ev: RawEvent,
) -> Option<String> {
    match ev {
        RawEvent::Book { asset_id, bids, asks } => {
            let asset_id = canonical_clob_token_id(&asset_id).into_owned();
            let entry = books.entry(asset_id.clone()).or_default();
            entry.0.clear();
            entry.1.clear();
            for l in &bids {
                if let (Ok(p), Ok(s)) = (l.price.parse::<f64>(), l.size.parse::<f64>()) {
                    entry.0.insert(price_to_key(p), s);
                }
            }
            for l in &asks {
                if let (Ok(p), Ok(s)) = (l.price.parse::<f64>(), l.size.parse::<f64>()) {
                    entry.1.insert(price_to_key(p), s);
                }
            }
            Some(asset_id)
        }
        RawEvent::PriceChange { asset_id, changes } => {
            let asset_id = canonical_clob_token_id(&asset_id).into_owned();
            let entry = books.entry(asset_id.clone()).or_default();
            for c in &changes {
                let (Ok(p), Ok(s)) = (c.price.parse::<f64>(), c.size.parse::<f64>()) else {
                    continue;
                };
                let key = price_to_key(p);
                let map = if c.side == "BUY" { &mut entry.0 } else { &mut entry.1 };
                if s == 0.0 {
                    map.remove(&key);
                } else {
                    map.insert(key, s);
                }
            }
            Some(asset_id)
        }
        RawEvent::Other => None,
    }
}

async fn send_snapshot(
    asset_id: &str,
    (bids, asks): &(BTreeMap<i64, f64>, BTreeMap<i64, f64>),
    out_bids: &mut Vec<BookLevel>,
    out_asks: &mut Vec<BookLevel>,
    tx: &mpsc::Sender<BookSnapshot>,
) {
    // bids: high→low, asks: low→high
    out_bids.clear();
    out_asks.clear();
    out_bids.reserve(bids.len().min(4096));
    out_asks.reserve(asks.len().min(4096));
    out_bids.extend(bids.iter().rev().map(|(p, s)| BookLevel {
        price: key_to_price(*p),
        size: *s,
    }));
    out_asks.extend(asks.iter().map(|(p, s)| BookLevel {
        price: key_to_price(*p),
        size: *s,
    }));
    // Swap the filled `Vec` out, leave an empty one with headroom to reduce realloc on the next
    // snapshot in this WS session.
    let (nb, na) = (out_bids.len(), out_asks.len());
    let b_cap = (nb * 2).clamp(8, 4096);
    let a_cap = (na * 2).clamp(8, 4096);
    let to_send_b = std::mem::replace(out_bids, Vec::with_capacity(b_cap));
    let to_send_a = std::mem::replace(out_asks, Vec::with_capacity(a_cap));
    let _ = tx
        .send(BookSnapshot {
            asset_id: asset_id.to_string(),
            bids:     to_send_b,
            asks:     to_send_a,
        })
        .await;
}
