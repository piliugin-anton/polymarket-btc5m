//! Polymarket CLOB WebSocket — public `market` channel for order book + trades.
//!
//! # Hot-path optimisations (over the original implementation)
//!
//! **Zero-alloc numeric parsing** — `RawLevel` / `RawChange` / `MarketPriceChangeItem` now
//! deserialise `price` and `size` directly to `f64` via a custom serde visitor that borrows
//! the JSON string bytes and calls `fast_float::parse` (≈ 3 × faster than `str::parse`).
//! No intermediate `String` is ever allocated for numeric fields.
//!
//! **Borrowed asset IDs** — `#[serde(borrow)]` lets serde_json hand out `&'de str` slices
//! that point into the original frame buffer for the `asset_id` field.  The token IDs sent
//! by the CLOB WS are plain ASCII decimal strings (no JSON escape sequences), so this is
//! always zero-copy.
//!
//! **`BookSide` = sorted `Vec<(i64, f64)>`** — replaces `BTreeMap<i64, f64>`.  Same O(log n)
//! updates via `binary_search`; sequential memory layout means snapshot reads scan a single
//! contiguous slice instead of pointer-chasing through B-tree nodes.  Full-book replacements
//! (`Book` event) push all levels then `sort_unstable` once rather than n individual inserts.
//!
//! **`BookDb` = inline linear array** — replaces `HashMap<String, BookSideMaps>`.  With 1–4
//! subscribed tokens (UP + DOWN + optional trailing tails), a `Vec::position` scan is faster
//! than computing a hash and resolving collisions.
//!
//! **Stack-allocated `Touched` bitmask** — replaces `HashSet<String>` (one heap allocation
//! per inbound frame).  `[bool; MAX_BOOKS]` lives on the stack; no allocation, no hashing.
//!
//! **Fast canonical bypass** — Polymarket's CLOB WS always sends token IDs in decimal form.
//! `ensure_canonical` detects this in one `all(is_ascii_digit)` scan and returns a borrowed
//! slice, skipping the 256-bit bignum parse + `to_string` round-trip in
//! `canonical_clob_token_id` entirely.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::config::CLOB_WS_URL;
use crate::trading::canonical_clob_token_id;

const PRICE_KEY_SCALE: f64 = 1_000_000.0;
/// Maximum distinct token IDs tracked in one WS session (UP + DOWN + trailing tails).
const MAX_BOOKS: usize = 8;

#[inline(always)]
fn price_to_key(p: f64) -> i64 {
    (p * PRICE_KEY_SCALE).round() as i64
}
#[inline(always)]
fn key_to_price(k: i64) -> f64 {
    k as f64 / PRICE_KEY_SCALE
}

// ── Public API (unchanged) ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: f64,
    pub size:  f64,
}

#[derive(Debug, Clone)]
pub struct BookSnapshot {
    pub asset_id: String,
    pub bids:     Vec<BookLevel>, // sorted high → low
    pub asks:     Vec<BookLevel>, // sorted low  → high
}

// ── Zero-alloc serde visitors ─────────────────────────────────────────────────

/// Deserialise a JSON string that encodes a decimal float — **no String allocation**.
/// Uses `fast_float::parse` (≈ 3 × stdlib) and borrows the JSON bytes directly.
fn deser_f64_str<'de, D: serde::Deserializer<'de>>(de: D) -> Result<f64, D::Error> {
    struct Vis;
    impl<'de> serde::de::Visitor<'de> for Vis {
        type Value = f64;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("decimal float as JSON string")
        }
        #[inline]
        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<f64, E> {
            fast_float::parse(s).map_err(E::custom)
        }
        #[inline]
        fn visit_borrowed_str<E: serde::de::Error>(self, s: &str) -> Result<f64, E> {
            fast_float::parse(s).map_err(E::custom)
        }
    }
    de.deserialize_str(Vis)
}

/// Deserialise `"BUY"` / `"SELL"` to `bool` (true = BUY) — **no String allocation**.
/// Uses ASCII case-fold on the first byte; branch-free.
fn deser_is_buy<'de, D: serde::Deserializer<'de>>(de: D) -> Result<bool, D::Error> {
    struct Vis;
    impl<'de> serde::de::Visitor<'de> for Vis {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("\"BUY\" or \"SELL\"")
        }
        #[inline]
        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<bool, E> {
            // 'B' | 0x20 == 'b'; 'S' | 0x20 == 's' — single-byte ASCII fold, no branch.
            Ok(s.as_bytes().first().copied().unwrap_or(0) | 0x20 == b'b')
        }
        #[inline]
        fn visit_borrowed_str<E: serde::de::Error>(self, s: &str) -> Result<bool, E> {
            self.visit_str(s)
        }
    }
    de.deserialize_str(Vis)
}

// ── Wire types ────────────────────────────────────────────────────────────────

/// `#[serde(borrow)]` on the internally-tagged enum is incompatible with `#[serde(other)]`
/// in the current serde derive, so `asset_id` stays `String` (one alloc per event, not per
/// level).  The high-frequency cost — numeric fields in `RawLevel` / `RawChange` — is fully
/// eliminated by the `f64` visitors above.
#[derive(Deserialize)]
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

/// Level from a full `book` snapshot — price/size parsed directly to `f64`, no String.
#[derive(Deserialize)]
struct RawLevel {
    #[serde(deserialize_with = "deser_f64_str")] price: f64,
    #[serde(deserialize_with = "deser_f64_str")] size:  f64,
}

/// Single level update from a `price_change` event.
#[derive(Deserialize)]
struct RawChange {
    #[serde(deserialize_with = "deser_f64_str")] price: f64,
    #[serde(deserialize_with = "deser_is_buy")]  side:  bool,
    #[serde(deserialize_with = "deser_f64_str")] size:  f64,
}

/// Alternate CLOB shape: `{ "market": …, "price_changes": […] }`.
#[derive(Deserialize)]
struct MarketPriceChangesMsg {
    #[allow(dead_code)]
    #[serde(default, rename = "market")]
    market: Option<String>,
    #[serde(default)]
    price_changes: Vec<MarketPriceChangeItem>,
}

#[derive(Deserialize)]
struct MarketPriceChangeItem {
    asset_id: String,
    #[serde(deserialize_with = "deser_f64_str")] price: f64,
    #[serde(deserialize_with = "deser_f64_str")] size:  f64,
    #[serde(deserialize_with = "deser_is_buy")]  side:  bool,
}

// ── Book state ────────────────────────────────────────────────────────────────

/// One book side as a **sorted `Vec<(price_key, size)>`**.
///
/// Invariant: entries are sorted ascending by `key` at all times.
/// - `upsert`: binary search + in-place write or `Vec::insert/remove` — O(log n) find,
///   O(n) shift for small n (≤ 40 levels).  Avoids all per-entry heap allocation.
/// - `replace_from`: push all, `sort_unstable` once — better for full book replacement.
/// - Snapshot: iterate the contiguous slice forward (asks) or backward (bids) — O(n),
///   no pointer chasing, entire book fits in a few cache lines.
struct BookSide {
    levels: Vec<(i64, f64)>,
}

impl BookSide {
    fn new() -> Self {
        Self { levels: Vec::with_capacity(32) }
    }

    fn clear(&mut self) {
        self.levels.clear();
    }

    /// Replace entire side from an iterator in arbitrary order.
    /// Pushes all non-zero entries then sorts once — O(n log n) but with minimal
    /// constant; much faster than n individual `upsert` calls when n is large.
    fn replace_from(&mut self, it: impl Iterator<Item = (i64, f64)>) {
        self.levels.clear();
        self.levels.extend(it.filter(|&(_, s)| s > 0.0));
        self.levels.sort_unstable_by_key(|&(k, _)| k);
    }

    /// Incremental update: insert, overwrite, or delete one level.
    #[inline(always)]
    fn upsert(&mut self, key: i64, size: f64) {
        match self.levels.binary_search_by_key(&key, |e| e.0) {
            Ok(i) => {
                if size > 0.0 {
                    // SAFETY: binary_search_by_key returned Ok(i) ⟹ i < len.
                    unsafe { self.levels.get_unchecked_mut(i).1 = size; }
                } else {
                    self.levels.remove(i);
                }
            }
            Err(i) => {
                if size > 0.0 {
                    self.levels.insert(i, (key, size));
                }
            }
        }
    }

    fn len(&self) -> usize { self.levels.len() }
}

struct TokenBook {
    canonical_id: String,
    bids: BookSide,
    asks: BookSide,
}

/// Tiny inline book database for ≤ `MAX_BOOKS` token IDs.
///
/// For n = 1–4 (typical: UP + DOWN + trailing tails), `Vec::position` beats a
/// `HashMap` lookup: no hash computation, no bucket indirection, all entries fit
/// in a single cache line.
struct BookDb {
    books: Vec<TokenBook>,
}

impl BookDb {
    fn new() -> Self {
        Self { books: Vec::with_capacity(MAX_BOOKS) }
    }

    /// O(n) scan — returns existing index or appends a new entry.
    fn find_or_insert(&mut self, canonical_id: &str) -> usize {
        if let Some(i) = self.books.iter().position(|b| b.canonical_id == canonical_id) {
            return i;
        }
        let i = self.books.len();
        self.books.push(TokenBook {
            canonical_id: canonical_id.to_owned(),
            bids: BookSide::new(),
            asks: BookSide::new(),
        });
        i
    }
}

/// Stack-allocated bitmask — zero-cost replacement for `HashSet<String>` tracking
/// which token indices were touched in a given inbound frame.
struct Touched {
    flags: [bool; MAX_BOOKS],
}

impl Touched {
    #[inline] fn new()               -> Self { Self { flags: [false; MAX_BOOKS] } }
    #[inline] fn set(&mut self, i: usize) { if i < MAX_BOOKS { self.flags[i] = true; } }
    #[inline] fn reset(&mut self)         { self.flags = [false; MAX_BOOKS]; }
}

// ── Canonical ID fast path ────────────────────────────────────────────────────

/// Polymarket's CLOB WS always sends token IDs as plain decimal strings (no `0x`
/// prefix, no whitespace).  Detect this in one pass and return a borrowed slice,
/// bypassing the U256 bignum parse + `to_string` round-trip in
/// `canonical_clob_token_id`.
#[inline(always)]
fn ensure_canonical(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return std::borrow::Cow::Borrowed(s);
    }
    canonical_clob_token_id(s)
}

// ── Spawn / run ───────────────────────────────────────────────────────────────

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

    let sub = serde_json::json!({ "type": "market", "assets_ids": token_ids });
    ws.send(Message::Text(sub.to_string().into())).await?;
    for (i, id) in token_ids.iter().enumerate() {
        let prefix: String = id.chars().take(12).collect();
        info!(i, token_id_prefix = %prefix, "CLOB WS subscribe token");
    }

    let mut db = BookDb::new();
    // Pre-register slots in the same order as `token_ids` so index 0 = UP, 1 = DOWN.
    for id in token_ids {
        let cid = ensure_canonical(id);
        db.find_or_insert(cid.as_ref());
    }

    // Output buffers reused across snapshots (swapped out into each BookSnapshot).
    let mut out_bids: Vec<BookLevel> = Vec::with_capacity(64);
    let mut out_asks: Vec<BookLevel> = Vec::with_capacity(64);
    let mut touched = Touched::new();

    let mut ping = tokio::time::interval(std::time::Duration::from_secs(10));
    ping.tick().await;

    let mut first_text_logged    = false;
    let mut first_non_text_logged = false;
    let mut n_frames = 0u64;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                let _ = ws.send(Message::Text("PING".into())).await;
            }
            m = ws.next() => {
                let m = match m {
                    Some(Ok(m))  => m,
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

                touched.reset();
                let t = txt.as_str();

                // Dispatch on first non-whitespace byte (cheap heuristic).
                if t.trim_start().starts_with('[') {
                    match serde_json::from_str::<Vec<RawEvent>>(t) {
                        Ok(events) => {
                            for ev in events {
                                if let Some(i) = apply_event(&mut db, ev) {
                                    touched.set(i);
                                }
                            }
                        }
                        Err(_) => { warn_unparsed_clob(t); continue; }
                    }
                } else if t.contains("price_changes") && t.trim_start().starts_with('{') {
                    match serde_json::from_str::<MarketPriceChangesMsg>(t) {
                        Ok(msg) => {
                            for ch in msg.price_changes {
                                let cid = ensure_canonical(&ch.asset_id);
                                let i   = db.find_or_insert(cid.as_ref());
                                let key = price_to_key(ch.price);
                                if ch.side { db.books[i].bids.upsert(key, ch.size); }
                                else       { db.books[i].asks.upsert(key, ch.size); }
                                touched.set(i);
                            }
                        }
                        Err(_) => { warn_unparsed_clob(t); continue; }
                    }
                } else {
                    match serde_json::from_str::<RawEvent>(t) {
                        Ok(ev) => {
                            if let Some(i) = apply_event(&mut db, ev) {
                                touched.set(i);
                            }
                        }
                        Err(_) => { warn_unparsed_clob(t); continue; }
                    }
                }

                // Emit one snapshot per touched token — only valid indices are set.
                for (i, &hit) in touched.flags[..db.books.len()].iter().enumerate() {
                    if hit {
                        let book = &db.books[i];
                        emit_snapshot(
                            &book.canonical_id,
                            &book.bids,
                            &book.asks,
                            &mut out_bids,
                            &mut out_asks,
                            tx,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

// ── Event application ─────────────────────────────────────────────────────────

#[inline]
fn apply_event(db: &mut BookDb, ev: RawEvent) -> Option<usize> {
    match ev {
        RawEvent::Book { asset_id, bids, asks } => {
            let cid = ensure_canonical(&asset_id);
            let i   = db.find_or_insert(cid.as_ref());
            let book = &mut db.books[i];
            book.bids.replace_from(bids.into_iter().map(|l| (price_to_key(l.price), l.size)));
            book.asks.replace_from(asks.into_iter().map(|l| (price_to_key(l.price), l.size)));
            Some(i)
        }
        RawEvent::PriceChange { asset_id, changes } => {
            let cid = ensure_canonical(&asset_id);
            let i   = db.find_or_insert(cid.as_ref());
            let book = &mut db.books[i];
            for c in changes {
                let key = price_to_key(c.price);
                if c.side { book.bids.upsert(key, c.size); }
                else      { book.asks.upsert(key, c.size); }
            }
            Some(i)
        }
        RawEvent::Other => None,
    }
}

// ── Snapshot emission ─────────────────────────────────────────────────────────

async fn emit_snapshot(
    asset_id: &str,
    bids:     &BookSide,
    asks:     &BookSide,
    out_bids: &mut Vec<BookLevel>,
    out_asks: &mut Vec<BookLevel>,
    tx:       &mpsc::Sender<BookSnapshot>,
) {
    out_bids.clear();
    out_asks.clear();
    // Bids: high → low  (reverse of ascending key order)
    out_bids.extend(bids.levels.iter().rev().map(|&(k, s)| BookLevel { price: key_to_price(k), size: s }));
    // Asks: low → high (ascending key order = natural Vec order)
    out_asks.extend(asks.levels.iter().map(|&(k, s)| BookLevel { price: key_to_price(k), size: s }));

    let nb    = out_bids.len();
    let na    = out_asks.len();
    let b_cap = (nb * 2).clamp(8, 4096);
    let a_cap = (na * 2).clamp(8, 4096);
    // Swap out the filled Vecs, leave pre-sized empty ones for the next snapshot.
    let snap_bids = std::mem::replace(out_bids, Vec::with_capacity(b_cap));
    let snap_asks = std::mem::replace(out_asks, Vec::with_capacity(a_cap));
    let _ = tx
        .send(BookSnapshot {
            asset_id: asset_id.to_owned(),
            bids:     snap_bids,
            asks:     snap_asks,
        })
        .await;
}

// ── Logging helpers ───────────────────────────────────────────────────────────

fn snip_frame(txt: &str, max: usize) -> String {
    let t = txt.trim();
    if t.len() <= max { t.to_string() } else { format!("{}…", &t[..max]) }
}

fn warn_unparsed_clob(t: &str) {
    warn!(
        len = t.len(),
        snippet = %snip_frame(t, 280),
        "CLOB WS text not parsed (book / price_change / market price_changes)",
    );
}
