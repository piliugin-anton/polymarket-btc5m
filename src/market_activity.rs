//! Rolling **60s wall-clock** traded notional (`price * size`) for public CLOB prints.

use std::collections::VecDeque;

/// Match [`docs`](https://docs.polymarket.com/market-data/websocket/market-channel): one headline window.
pub const ACTIVITY_WINDOW_MS: i64 = 60_000;

/// Data API may return seconds or millis; WS uses millis (13-digit typical).
#[inline]
pub fn normalize_exchange_ts_ms(ts: i64) -> i64 {
    if ts > 0 && ts < 1_000_000_000_000 {
        ts.saturating_mul(1000)
    } else {
        ts
    }
}

#[derive(Debug, Default)]
pub struct RollingTradedNotional {
    deque: VecDeque<(i64, f64)>,
    sum: f64,
}

impl RollingTradedNotional {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.deque.clear();
        self.sum = 0.0;
    }

    fn prune_before(&mut self, cutoff_ms: i64) {
        while let Some(&(t, n)) = self.deque.front() {
            if t < cutoff_ms {
                self.sum -= n;
                self.deque.pop_front();
            } else {
                break;
            }
        }
    }

    /// Add one executed print using the exchange `timestamp` (ms).
    pub fn record_trade(&mut self, ts_ms: i64, notional: f64) {
        if !notional.is_finite() || notional <= 0.0 {
            return;
        }
        let cutoff = ts_ms.saturating_sub(ACTIVITY_WINDOW_MS);
        self.prune_before(cutoff);
        self.deque.push_back((ts_ms, notional));
        self.sum += notional;
    }

    /// Drop entries older than `now_wall_ms - window` so the total decays when the feed is quiet.
    pub fn prune_against_wall_clock(&mut self, now_wall_ms: i64) {
        let cutoff = now_wall_ms.saturating_sub(ACTIVITY_WINDOW_MS);
        self.prune_before(cutoff);
    }

    pub fn total(&self) -> f64 {
        self.sum
    }
}

/// Compact notional for the status line (USDC-equivalent scale; shares × price in [0,1] markets).
pub fn format_compact_notional(n: f64) -> String {
    if !n.is_finite() || n < 0.0 {
        return "—".into();
    }
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else if n >= 100.0 {
        format!("{:.0}", n)
    } else if n >= 10.0 {
        format!("{:.1}", n)
    } else {
        format!("{:.2}", n)
    }
}
