//! Prevents the same CLOB **fill** from updating [`crate::app::AppState`] twice when both
//! [`crate::app::AppEvent::OrderAck`] and a user WebSocket `trade` describe one execution.
//!
//! 1) Every CLOB `Trade.id` is stored in `seen_trades` (hydration JSON + every applied WS event).
//! 2) `ack_wait` / `ws_wait` short-lived queues match `(clob order id, qty, price)` when the two
//!    channels race (POST /order does not return `Trade.id`).
//!
//! Shared across the TUI event loop, the user-channel WebSocket task, and the market supervisor.
//! [`tokio::sync::Mutex`] is used so waiting tasks yield instead of blocking a runtime worker
//! (unlike `std::sync::Mutex`). Atomics are not a fit here: we need a set plus bounded wait lists.

use std::fmt;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::trading::norm_order_id_key;

const MATCH_TTL: Duration = Duration::from_secs(8);
const MAX_WAIT: usize = 32;

#[derive(Debug, Clone)]
struct WaitSlot {
    oid:     String, // norm
    qty:     f64,
    price:   f64,
    created: Instant,
}

/// Cross-channel fill de-duplication for OrderAck vs user WSS `trade`.
pub struct UserTradeSync {
    inner: Mutex<Inner>,
}

impl fmt::Debug for UserTradeSync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserTradeSync { .. }")
    }
}

struct Inner {
    seen_trades:  std::collections::HashSet<String>,
    ack_wait:     Vec<WaitSlot>,
    ws_wait:      Vec<WaitSlot>,
}

impl UserTradeSync {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                seen_trades:  std::collections::HashSet::new(),
                ack_wait:     Vec::new(),
                ws_wait:      Vec::new(),
            }),
        }
    }

    pub async fn on_market_roll(&self) {
        let mut g = self.inner.lock().await;
        g.seen_trades.clear();
        g.ack_wait.clear();
        g.ws_wait.clear();
    }

    /// Trade ids from `GET /data/trades` — later WS re-pushes are ignored.
    pub async fn seed_seen_from_hydration(&self, trade_ids: impl IntoIterator<Item = String>) {
        let mut g = self.inner.lock().await;
        for id in trade_ids {
            if !id.is_empty() {
                g.seen_trades.insert(id);
            }
        }
    }

    /// `true` = apply the OrderAck P&L block. `false` = user WSS `trade` already matched the fill.
    pub async fn before_order_ack_apply(&self, clob_order_id: &str, qty: f64, price: f64) -> bool {
        let o = norm_order_id_key(clob_order_id);
        if o.is_empty() {
            return true;
        }
        let mut g = self.inner.lock().await;
        prune_stale(&mut g.ack_wait);
        prune_stale(&mut g.ws_wait);
        if let Some(i) = g
            .ws_wait
            .iter()
            .position(|w| w.oid == o && close_enough(w.qty, qty) && close_enough(w.price, price))
        {
            g.ws_wait.remove(i);
            return false;
        }
        true
    }

    /// After a successful `OrderAck` P&L update.
    pub async fn after_order_ack_applied(&self, clob_order_id: &str, qty: f64, price: f64) {
        let o = norm_order_id_key(clob_order_id);
        if o.is_empty() {
            return;
        }
        let mut g = self.inner.lock().await;
        prune_stale(&mut g.ack_wait);
        push_cap(
            &mut g.ack_wait,
            WaitSlot { oid: o, qty, price, created: Instant::now() },
        );
    }

    /// `true` = forward this WSS `trade` to the TUI (no duplicate).
    pub async fn before_ws_user_fill_apply(
        &self,
        clob_trade_id: &str,
        taker_or_leg_id: &str,
        qty: f64,
        price: f64,
    ) -> bool {
        let o = norm_order_id_key(taker_or_leg_id);
        let mut g = self.inner.lock().await;
        if !clob_trade_id.is_empty() && g.seen_trades.contains(clob_trade_id) {
            return false;
        }
        if o.is_empty() {
            return true;
        }
        prune_stale(&mut g.ack_wait);
        prune_stale(&mut g.ws_wait);
        if let Some(i) = g.ack_wait.iter().position(|w| {
            w.oid == o && close_enough(w.qty, qty) && close_enough(w.price, price)
        }) {
            g.ack_wait.remove(i);
            if !clob_trade_id.is_empty() {
                g.seen_trades.insert(clob_trade_id.to_string());
            }
            return false;
        }
        true
    }

    /// After [`crate::app::AppEvent::UserChannelFill`] mutates `AppState`.
    pub async fn after_ws_user_fill_committed(
        &self,
        clob_trade_id: &str,
        taker_or_leg_id: &str,
        qty: f64,
        price: f64,
    ) {
        let o = norm_order_id_key(taker_or_leg_id);
        if o.is_empty() {
            return;
        }
        let mut g = self.inner.lock().await;
        if !clob_trade_id.is_empty() {
            g.seen_trades.insert(clob_trade_id.to_string());
        }
        prune_stale(&mut g.ack_wait);
        push_cap(
            &mut g.ws_wait,
            WaitSlot { oid: o, qty, price, created: Instant::now() },
        );
    }
}

fn close_enough(a: f64, b: f64) -> bool {
    if !a.is_finite() || !b.is_finite() {
        return false;
    }
    let d = (a - b).abs();
    let scale = a.abs().max(b.abs()).max(1.0);
    d < 1e-6_f64.max(1e-5 * scale)
}

fn prune_stale(v: &mut Vec<WaitSlot>) {
    v.retain(|s| s.created.elapsed() < MATCH_TTL);
}

fn push_cap(v: &mut Vec<WaitSlot>, slot: WaitSlot) {
    v.push(slot);
    while v.len() > MAX_WAIT {
        v.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ack_then_ws_dedupes() {
        let s = UserTradeSync::new();
        s.after_order_ack_applied("0xabc1", 10.0, 0.55).await;
        assert!(!s.before_ws_user_fill_apply("T-1", "0xabc1", 10.0, 0.55).await);
    }

    #[tokio::test]
    async fn ws_then_ack_dedupes() {
        let s = UserTradeSync::new();
        assert!(s.before_ws_user_fill_apply("T-2", "0xdef2", 5.0, 0.40).await);
        s.after_ws_user_fill_committed("T-2", "0xdef2", 5.0, 0.40).await;
        assert!(!s.before_order_ack_apply("0xdef2", 5.0, 0.40).await);
    }
}
