//! Prevents the same CLOB **fill** from updating [`crate::app::AppState`] twice when both
//! [`crate::app::AppEvent::OrderAck`] and a user WebSocket `trade` describe one execution.
//!
//! 1) Every CLOB `Trade.id` is stored in `seen_trades` (hydration JSON + every applied WS event).
//! 2) `ack_wait` / `ws_wait` short-lived queues match `(clob order id, qty, price)` when the two
//!    channels race (POST /order does not return `Trade.id`).
//!
//! **Event-loop guard** (fixes two races not covered by the WS-task pre-check):
//! - `fill_already_committed`: re-checks `seen_trades` inside the event loop so a WS reconnect
//!   that replays the same trade ID before `after_ws_user_fill_committed` completes is blocked.
//! - `ack_claimed_for_ws_fill`: checks `ack_wait` so an `OrderAck` that sneaked ahead of a queued
//!   `UserChannelFill` (TOCTOU between the WS-task pre-check and the event-loop commit) is detected.
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
    ///
    /// Claims `clob_trade_id` in `seen_trades` immediately on the forward path so a second
    /// delivery of the same CONFIRMED event (Polymarket re-sends on reconnect or flushes
    /// duplicates) is blocked right here in the WS task before it reaches the event queue.
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
        // Claim the trade ID now — a second CONFIRMED delivery is blocked immediately
        // without waiting for after_ws_user_fill_committed to run in the event loop.
        if !clob_trade_id.is_empty() {
            g.seen_trades.insert(clob_trade_id.to_string());
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

    /// Event-loop guard: `true` if this trade was already committed (WS reconnect replay race).
    ///
    /// Called at the top of the `UserChannelFill` handler — runs inside the single-threaded event
    /// loop, sequentially with `OrderAck`, so it sees `seen_trades` as it truly stands right now.
    pub async fn fill_already_committed(&self, clob_trade_id: &str) -> bool {
        if clob_trade_id.is_empty() {
            return false;
        }
        self.inner.lock().await.seen_trades.contains(clob_trade_id)
    }

    /// Event-loop guard: `true` if an `OrderAck` raced ahead of this queued `UserChannelFill`.
    ///
    /// Removes the matching `ack_wait` entry (consumed) and records the trade ID in `seen_trades`
    /// so a future WS reconnect replay of the same trade is also blocked.
    pub async fn ack_claimed_for_ws_fill(
        &self,
        taker_or_leg_id: &str,
        qty: f64,
        price: f64,
        clob_trade_id: &str,
    ) -> bool {
        let o = norm_order_id_key(taker_or_leg_id);
        if o.is_empty() {
            return false;
        }
        let mut g = self.inner.lock().await;
        prune_stale(&mut g.ack_wait);
        if let Some(i) = g
            .ack_wait
            .iter()
            .position(|w| w.oid == o && close_enough(w.qty, qty) && close_enough(w.price, price))
        {
            g.ack_wait.remove(i);
            if !clob_trade_id.is_empty() {
                g.seen_trades.insert(clob_trade_id.to_string());
            }
            return true;
        }
        false
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

    // Race: WS pre-check passes, OrderAck sneaks ahead in the event queue before the
    // UserChannelFill is processed. The event-loop guard must catch it.
    #[tokio::test]
    async fn event_loop_guard_ack_raced_ahead_of_ws_fill() {
        let s = UserTradeSync::new();
        // WS pre-check passes (no ack yet) and claims T-3 in seen_trades
        assert!(s.before_ws_user_fill_apply("T-3", "0xorder3", 8.0, 0.60).await);
        // OrderAck is processed first in the event loop before UserChannelFill is handled
        assert!(s.before_order_ack_apply("0xorder3", 8.0, 0.60).await);
        s.after_order_ack_applied("0xorder3", 8.0, 0.60).await;
        // Now UserChannelFill reaches the event-loop handler — ack_claimed_for_ws_fill catches it
        assert!(s.ack_claimed_for_ws_fill("0xorder3", 8.0, 0.60, "T-3").await);
        // trade ID already in seen_trades from the pre-check claim
        assert!(s.fill_already_committed("T-3").await);
    }

    // Same CONFIRMED event delivered twice (Polymarket WS re-sends): second is blocked
    // immediately in the WS task pre-check because the first claimed the trade ID.
    #[tokio::test]
    async fn ws_duplicate_confirmed_blocked_in_pretask() {
        let s = UserTradeSync::new();
        // First CONFIRMED delivery — claimed and forwarded
        assert!(s.before_ws_user_fill_apply("T-4", "0xorder4", 3.0, 0.45).await);
        // Second CONFIRMED delivery for same trade ID — blocked right here, never queued
        assert!(!s.before_ws_user_fill_apply("T-4", "0xorder4", 3.0, 0.45).await);
    }

    // Event-loop safety net: if two events somehow reach the queue, fill_already_committed
    // blocks the second after the first is committed.
    #[tokio::test]
    async fn event_loop_guard_catches_duplicate_in_queue() {
        let s = UserTradeSync::new();
        // Simulate first event being committed (seen_trades updated)
        s.after_ws_user_fill_committed("T-5", "0xorder5", 2.0, 0.30).await;
        // Second event reaches the event-loop handler
        assert!(s.fill_already_committed("T-5").await);
    }
}
