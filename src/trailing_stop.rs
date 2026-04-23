//! Lock-free trailing stop for HFT crypto trading.
//!
//! # Design
//! - [`TrailingStop`] is a pure in-memory state machine guarding one position.
//!   No allocations on the hot path. No mutexes. Feed it prices, react to the
//!   returned [`TickOutcome`].
//! - All mutable state is held in `AtomicU64` with `f64` bit-casts and CAS
//!   loops. Throughput on the hot path is limited by cache line contention,
//!   not by a lock.
//! - Venue-agnostic. Plug in Binance / Bybit / Drift / Aster / Jupiter / …
//!   by providing a price feed and an order executor on top.
//!
//! # Caveats for production use
//! - **For perps, feed mark price** — not last trade. Last-trade prints get
//!   wick-hunted in thin books; mark price smooths that out and is what the
//!   exchange uses for liquidation anyway.
//! - **Always place a server-side fail-safe stop** as a backup. Your process
//!   can crash, the WS can drop, your VPS can reboot. The client-side trail
//!   gives you logic; the exchange stop gives you survival.
//! - **Fire IOC limit through the book**, not naked market, on trigger. A
//!   market order is a slippage blank check; an aggressive IOC (a few ticks
//!   past the opposite top-of-book) gives you a bounded worst-case fill while
//!   still clearing in volatile conditions.
//! - **Prices are `f64`**. Fine for BTC/ETH/majors. If you're trading a
//!   long-tail token quoted as `0.0000001234`, switch to fixed-point (`i64`
//!   ticks with a per-market scalar) to avoid precision drift.
//!
//! The binary only uses a subset of this module (e.g. long, percent trail,
//! `Immediate`); the rest is public for reuse — suppress `dead_code` for it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Side of the underlying position the stop protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Long,
    Short,
}

/// How far behind the peak the stop trails.
#[derive(Debug, Clone, Copy)]
pub enum TrailSpec {
    /// Fractional offset from peak. `Percent(0.01)` = 1 % trail.
    Percent(f64),
    /// Absolute offset in quote units (e.g. USDC).
    Absolute(f64),
}

/// When the stop first becomes live.
#[derive(Debug, Clone, Copy)]
pub enum Activation {
    /// Armed at construction. Trails from the entry price immediately.
    Immediate,
    /// Armed once price first crosses this level in the favorable direction.
    /// Use to let winners run before arming protection.
    Price(f64),
    /// Armed once price has moved this many quote units in favor of the
    /// position, measured from entry.
    FavorableDistance(f64),
}

/// Result of processing one price tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TickOutcome {
    /// Nothing to do.
    NoOp,
    /// Activation crossed on this tick; stop is now live.
    Armed { stop_price: f64 },
    /// Peak moved and the stop was ratcheted.
    Trailed { new_stop: f64 },
    /// Stop breached. Caller should flatten now.
    Triggered { trigger_price: f64, stop_price: f64 },
    /// Trigger already fired on a previous tick — idempotent no-op.
    AlreadyTriggered,
}

/// Trailing stop guarding a single position.
#[derive(Debug)]
pub struct TrailingStop {
    side: Side,
    spec: TrailSpec,
    entry_price: f64,
    activation: Activation,

    /// Best favorable price since arming. `f64::NAN` sentinel = unarmed.
    best_price_bits: AtomicU64,
    /// Current stop price. `f64::NAN` sentinel = unarmed.
    stop_price_bits: AtomicU64,
    armed: AtomicBool,
    triggered: AtomicBool,
}

#[inline]
fn load_f64(a: &AtomicU64) -> f64 {
    f64::from_bits(a.load(Ordering::Acquire))
}

impl TrailingStop {
    pub fn new(
        side: Side,
        entry_price: f64,
        spec: TrailSpec,
        activation: Activation,
    ) -> Self {
        let nan = f64::NAN.to_bits();
        let stop = Self {
            side,
            spec,
            entry_price,
            activation,
            best_price_bits: AtomicU64::new(nan),
            stop_price_bits: AtomicU64::new(nan),
            armed: AtomicBool::new(matches!(activation, Activation::Immediate)),
            triggered: AtomicBool::new(false),
        };
        if matches!(activation, Activation::Immediate) {
            stop.best_price_bits
                .store(entry_price.to_bits(), Ordering::Release);
            stop.stop_price_bits
                .store(stop.compute_stop(entry_price).to_bits(), Ordering::Release);
        }
        stop
    }

    #[inline]
    fn compute_stop(&self, best: f64) -> f64 {
        let dist = match self.spec {
            TrailSpec::Percent(p) => best * p,
            TrailSpec::Absolute(d) => d,
        };
        match self.side {
            Side::Long => best - dist,
            Side::Short => best + dist,
        }
    }

    #[inline]
    fn activation_met(&self, price: f64) -> bool {
        match self.activation {
            Activation::Immediate => true,
            Activation::Price(p) => match self.side {
                Side::Long => price >= p,
                Side::Short => price <= p,
            },
            Activation::FavorableDistance(d) => match self.side {
                Side::Long => price - self.entry_price >= d,
                Side::Short => self.entry_price - price >= d,
            },
        }
    }

    #[inline]
    fn is_breached(&self, price: f64, stop: f64) -> bool {
        match self.side {
            Side::Long => price <= stop,
            Side::Short => price >= stop,
        }
    }

    #[inline]
    fn is_better(&self, new: f64, best: f64) -> bool {
        if best.is_nan() {
            return true;
        }
        match self.side {
            Side::Long => new > best,
            Side::Short => new < best,
        }
    }

    /// Hot path. Lock-free. Call on every price tick.
    pub fn on_price(&self, price: f64) -> TickOutcome {
        if self.triggered.load(Ordering::Acquire) {
            return TickOutcome::AlreadyTriggered;
        }

        // 1. Activation gate.
        if !self.armed.load(Ordering::Acquire) {
            if !self.activation_met(price) {
                return TickOutcome::NoOp;
            }
            // Race exactly one thread into the armed state.
            if self
                .armed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.best_price_bits.store(price.to_bits(), Ordering::Release);
                let stop = self.compute_stop(price);
                self.stop_price_bits.store(stop.to_bits(), Ordering::Release);
                return TickOutcome::Armed { stop_price: stop };
            }
            // Lost the race. Fall through — now armed by someone else.
        }

        // 2. Ratchet the peak via CAS. Monotonic in the favorable direction.
        let mut new_stop: Option<f64> = None;
        loop {
            let best_bits = self.best_price_bits.load(Ordering::Acquire);
            let best = f64::from_bits(best_bits);
            if !self.is_better(price, best) {
                break;
            }
            if self
                .best_price_bits
                .compare_exchange_weak(
                    best_bits,
                    price.to_bits(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                let candidate = self.compute_stop(price);
                // Publish the new stop, but don't overwrite a concurrently-set
                // better stop from a racing thread.
                loop {
                    let cur_bits = self.stop_price_bits.load(Ordering::Acquire);
                    let cur = f64::from_bits(cur_bits);
                    if !cur.is_nan() && !self.is_better(candidate, cur) {
                        break;
                    }
                    if self
                        .stop_price_bits
                        .compare_exchange_weak(
                            cur_bits,
                            candidate.to_bits(),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        new_stop = Some(candidate);
                        break;
                    }
                }
                break;
            }
        }

        // 3. Breach check against whatever stop is currently published.
        let stop = load_f64(&self.stop_price_bits);
        if !stop.is_nan() && self.is_breached(price, stop) {
            if self
                .triggered
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return TickOutcome::Triggered {
                    trigger_price: price,
                    stop_price: stop,
                };
            }
            return TickOutcome::AlreadyTriggered;
        }

        match new_stop {
            Some(s) => TickOutcome::Trailed { new_stop: s },
            None => TickOutcome::NoOp,
        }
    }

    pub fn side(&self) -> Side { self.side }
    pub fn entry_price(&self) -> f64 { self.entry_price }

    pub fn stop_price(&self) -> Option<f64> {
        let v = load_f64(&self.stop_price_bits);
        if v.is_nan() { None } else { Some(v) }
    }
    pub fn best_price(&self) -> Option<f64> {
        let v = load_f64(&self.best_price_bits);
        if v.is_nan() { None } else { Some(v) }
    }
    pub fn is_armed(&self) -> bool { self.armed.load(Ordering::Acquire) }
    pub fn is_triggered(&self) -> bool { self.triggered.load(Ordering::Acquire) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn long_immediate_basic() {
        let ts = TrailingStop::new(
            Side::Long,
            100.0,
            TrailSpec::Percent(0.02),
            Activation::Immediate,
        );
        assert!(ts.is_armed());
        assert!(approx_eq(ts.stop_price().unwrap(), 98.0));

        // Price climbs — stop ratchets up.
        match ts.on_price(110.0) {
            TickOutcome::Trailed { new_stop } => assert!(approx_eq(new_stop, 107.8)),
            o => panic!("expected Trailed, got {:?}", o),
        }
        // Pullback within trail — no-op.
        assert_eq!(ts.on_price(108.0), TickOutcome::NoOp);
        // Breach — trigger.
        match ts.on_price(107.5) {
            TickOutcome::Triggered { stop_price, .. } => {
                assert!(approx_eq(stop_price, 107.8))
            }
            o => panic!("expected Triggered, got {:?}", o),
        }
        // Idempotent.
        assert_eq!(ts.on_price(50.0), TickOutcome::AlreadyTriggered);
    }

    #[test]
    fn short_immediate_basic() {
        let ts = TrailingStop::new(
            Side::Short,
            100.0,
            TrailSpec::Absolute(2.0),
            Activation::Immediate,
        );
        assert!(approx_eq(ts.stop_price().unwrap(), 102.0));

        match ts.on_price(90.0) {
            TickOutcome::Trailed { new_stop } => assert!(approx_eq(new_stop, 92.0)),
            o => panic!("expected Trailed, got {:?}", o),
        }
        assert_eq!(ts.on_price(91.0), TickOutcome::NoOp);
        match ts.on_price(92.5) {
            TickOutcome::Triggered { .. } => {}
            o => panic!("expected Triggered, got {:?}", o),
        }
    }

    #[test]
    fn activation_by_price_long() {
        let ts = TrailingStop::new(
            Side::Long,
            100.0,
            TrailSpec::Percent(0.02),
            Activation::Price(105.0),
        );
        assert!(!ts.is_armed());
        assert_eq!(ts.on_price(103.0), TickOutcome::NoOp);
        match ts.on_price(106.0) {
            TickOutcome::Armed { stop_price } => {
                assert!(approx_eq(stop_price, 106.0 * 0.98))
            }
            o => panic!("expected Armed, got {:?}", o),
        }
        assert!(ts.is_armed());
    }

    #[test]
    fn activation_by_favorable_distance_short() {
        let ts = TrailingStop::new(
            Side::Short,
            100.0,
            TrailSpec::Absolute(1.0),
            Activation::FavorableDistance(3.0),
        );
        assert_eq!(ts.on_price(98.0), TickOutcome::NoOp);
        match ts.on_price(96.5) {
            TickOutcome::Armed { stop_price } => assert!(approx_eq(stop_price, 97.5)),
            o => panic!("expected Armed, got {:?}", o),
        }
    }

    #[test]
    fn gap_triggers_on_first_tick_after_arming() {
        let ts = TrailingStop::new(
            Side::Long,
            100.0,
            TrailSpec::Absolute(2.0),
            Activation::Immediate,
        );
        // Gap straight through the stop.
        match ts.on_price(95.0) {
            TickOutcome::Triggered { trigger_price, stop_price } => {
                assert!(approx_eq(trigger_price, 95.0));
                assert!(approx_eq(stop_price, 98.0));
            }
            o => panic!("expected Triggered, got {:?}", o),
        }
    }
}