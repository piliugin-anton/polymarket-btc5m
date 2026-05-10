//! Lock-free trailing stop for HFT crypto trading.
//!
//! # Design
//! - [`TrailingStop`] is a pure in-memory state machine guarding one position.
//!   No allocations on the hot path. No mutexes. Feed it prices, react to the
//!   returned [`TickOutcome`].
//! - Peak and stop are held as **integer price ticks** (`0.01` per tick) in
//!   `AtomicI64` with CAS loops. No `f64` in the hot ratchet. Throughput is
//!   still limited by cache line contention, not a lock.
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
//! - The public API still takes `f64` prices. Internally the contract assumes
//!   a **0.01 quote tick** (same as many USDⓈ-quoted perps/spot). Stops and
//!   best track are rounded to that grid; for sub-tick instruments, lower
//!   `TICK` or a separate fixed-point type.
//!
//! The binary only uses a subset of this module (e.g. long, percent trail,
//! `Immediate`); the rest is public for reuse — suppress `dead_code` for it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Quote tick in price units. One `i64` step is one `TICK` of price.
pub const TICK: f64 = 0.01;

/// Unarmed or unset: no best/stop tick stored yet.
pub const UNSET: i64 = i64::MIN;

#[inline]
pub fn to_tick(p: f64) -> i64 {
    (p / TICK).round() as i64
}

#[inline]
pub const fn from_tick(t: i64) -> f64 {
    t as f64 * TICK
}

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

    /// Best favorable **tick** since arming; [`UNSET`] if not set.
    best_tick: AtomicI64,
    /// Current stop **tick**; [`UNSET`] if not set.
    stop_tick: AtomicI64,
    armed: AtomicBool,
    triggered: AtomicBool,
}

impl TrailingStop {
    pub fn new(side: Side, entry_price: f64, spec: TrailSpec, activation: Activation) -> Self {
        let best = to_tick(entry_price);
        let (initial_best, initial_stop) = if matches!(activation, Activation::Immediate) {
            let s = Self::stop_tick_for_best(side, &spec, best);
            (best, s)
        } else {
            (UNSET, UNSET)
        };
        Self {
            side,
            spec,
            entry_price,
            activation,
            best_tick: AtomicI64::new(initial_best),
            stop_tick: AtomicI64::new(initial_stop),
            armed: AtomicBool::new(matches!(activation, Activation::Immediate)),
            triggered: AtomicBool::new(false),
        }
    }

    /// Stop tick for a given best tick. Quantizes trail distance to at least one full tick.
    #[inline]
    pub(crate) fn stop_tick_for_best(side: Side, spec: &TrailSpec, best: i64) -> i64 {
        let dist_ticks = match *spec {
            TrailSpec::Percent(p) if p > 0.0 => (((best as f64).abs() * p).ceil() as i64).max(1),
            TrailSpec::Absolute(d) if d > 0.0 => (((d / TICK) - 1e-12).ceil() as i64).max(1),
            _ => 0,
        };
        match side {
            Side::Long => best - dist_ticks,
            Side::Short => best + dist_ticks,
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
    fn is_breached_tick(&self, price_tick: i64, stop_tick: i64) -> bool {
        if stop_tick == UNSET {
            return false;
        }
        match self.side {
            Side::Long => price_tick <= stop_tick,
            Side::Short => price_tick >= stop_tick,
        }
    }

    /// More favorable *underlying* print than the stored best.
    #[inline]
    fn is_better_tick(&self, new_tick: i64, best_tick: i64) -> bool {
        if best_tick == UNSET {
            return true;
        }
        match self.side {
            Side::Long => new_tick > best_tick,
            Side::Short => new_tick < best_tick,
        }
    }

    /// Favorable move for a *trailing* stop: higher is better (long) / lower (short).
    #[inline]
    fn is_favorable_stop_vs(&self, new_stop: i64, cur: i64) -> bool {
        if cur == UNSET {
            return true;
        }
        match self.side {
            Side::Long => new_stop > cur,
            Side::Short => new_stop < cur,
        }
    }

    /// Hot path. Lock-free. Call on every price tick.
    pub fn on_price(&self, price: f64) -> TickOutcome {
        if self.triggered.load(Ordering::Acquire) {
            return TickOutcome::AlreadyTriggered;
        }

        let price_tick = to_tick(price);

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
                self.best_tick.store(price_tick, Ordering::Release);
                let stop_t = Self::stop_tick_for_best(self.side, &self.spec, price_tick);
                self.stop_tick.store(stop_t, Ordering::Release);
                return TickOutcome::Armed {
                    stop_price: from_tick(stop_t),
                };
            }
            // Lost the race. Fall through — now armed by someone else.
        }

        // 2. Ratchet the peak via CAS. Monotonic in the favorable direction.
        let mut new_stop: Option<i64> = None;
        loop {
            let best = self.best_tick.load(Ordering::Acquire);
            if !self.is_better_tick(price_tick, best) {
                break;
            }
            if self
                .best_tick
                .compare_exchange_weak(best, price_tick, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let candidate = Self::stop_tick_for_best(self.side, &self.spec, price_tick);
                // Publish the new stop, but don't overwrite a concurrently-set
                // better stop from a racing thread.
                loop {
                    let cur = self.stop_tick.load(Ordering::Acquire);
                    if cur != UNSET && !self.is_favorable_stop_vs(candidate, cur) {
                        break;
                    }
                    if self
                        .stop_tick
                        .compare_exchange_weak(cur, candidate, Ordering::AcqRel, Ordering::Acquire)
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
        let st = self.stop_tick.load(Ordering::Acquire);
        // If we just ratcheted, the new stop may round to the *same* tick as the
        // last print. That is not a “through the stop” breach; skip until the
        // next tick, otherwise a favorable move can spuriously trigger.
        if st != UNSET
            && self.is_breached_tick(price_tick, st)
            && !matches!(&new_stop, Some(n) if *n == st && *n == price_tick)
        {
            if self
                .triggered
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return TickOutcome::Triggered {
                    trigger_price: price,
                    stop_price: from_tick(st),
                };
            }
            return TickOutcome::AlreadyTriggered;
        }

        match new_stop {
            Some(s) => TickOutcome::Trailed {
                new_stop: from_tick(s),
            },
            None => TickOutcome::NoOp,
        }
    }

    pub fn side(&self) -> Side {
        self.side
    }
    pub fn entry_price(&self) -> f64 {
        self.entry_price
    }

    pub fn stop_price(&self) -> Option<f64> {
        let t = self.stop_tick.load(Ordering::Acquire);
        if t == UNSET {
            None
        } else {
            Some(from_tick(t))
        }
    }
    pub fn best_price(&self) -> Option<f64> {
        let t = self.best_tick.load(Ordering::Acquire);
        if t == UNSET {
            None
        } else {
            Some(from_tick(t))
        }
    }
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }
    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected `stop_price` when `best` prints at `at_price` (0.01 grid).
    fn want_stop_at(side: Side, spec: TrailSpec, at_price: f64) -> f64 {
        from_tick(TrailingStop::stop_tick_for_best(
            side,
            &spec,
            to_tick(at_price),
        ))
    }

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Tolerance for prices in ~0.01–1.0 range (percent trails, small absolute offsets).
    fn approx_eq_frac(a: f64, b: f64) -> bool {
        let scale = a.abs().max(b.abs()).max(0.01);
        (a - b).abs() < 1e-12_f64.max(1e-9 * scale)
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
            TickOutcome::Triggered {
                trigger_price,
                stop_price,
            } => {
                assert!(approx_eq(trigger_price, 95.0));
                assert!(approx_eq(stop_price, 98.0));
            }
            o => panic!("expected Triggered, got {:?}", o),
        }
    }

    // --- Prices in 0.01–0.99 (fractional quote / low-tick instruments) ---

    #[test]
    fn frac_long_immediate_percent_trail_and_trigger() {
        let entry = 0.50;
        let pct = 0.02;
        let ts = TrailingStop::new(
            Side::Long,
            entry,
            TrailSpec::Percent(pct),
            Activation::Immediate,
        );
        assert!(ts.is_armed());
        let initial_stop = want_stop_at(Side::Long, TrailSpec::Percent(pct), entry);
        assert!(
            approx_eq_frac(ts.stop_price().unwrap(), initial_stop),
            "stop={:?} want {}",
            ts.stop_price(),
            initial_stop
        );

        let peak = 0.66;
        match ts.on_price(peak) {
            TickOutcome::Trailed { new_stop } => {
                let want = want_stop_at(Side::Long, TrailSpec::Percent(pct), peak);
                assert!(
                    approx_eq_frac(new_stop, want),
                    "got {} want {}",
                    new_stop,
                    want
                );
            }
            o => panic!("expected Trailed, got {:?}", o),
        }

        let new_stop = ts.stop_price().unwrap();
        assert!(approx_eq_frac(
            new_stop,
            want_stop_at(Side::Long, TrailSpec::Percent(pct), peak)
        ));

        // Pullback above the quantized stop but still below the peak; 0.656
        // quantizes to the same best tick as 0.66 without improving the best.
        let mid = 0.656;
        assert!(mid > new_stop && mid < peak);
        assert_eq!(ts.on_price(mid), TickOutcome::NoOp);

        let breach = 0.63;
        assert!(breach < new_stop);
        match ts.on_price(breach) {
            TickOutcome::Triggered {
                trigger_price,
                stop_price,
            } => {
                assert!(approx_eq_frac(trigger_price, breach));
                assert!(approx_eq_frac(stop_price, new_stop));
            }
            o => panic!("expected Triggered, got {:?}", o),
        }
        assert_eq!(ts.on_price(0.01), TickOutcome::AlreadyTriggered);
    }

    #[test]
    fn frac_short_immediate_absolute_trail_and_trigger() {
        let entry = 0.55;
        let abs_off = 0.02;
        let ts = TrailingStop::new(
            Side::Short,
            entry,
            TrailSpec::Absolute(abs_off),
            Activation::Immediate,
        );
        assert!(approx_eq_frac(ts.stop_price().unwrap(), entry + abs_off));

        let low = 0.22;
        match ts.on_price(low) {
            TickOutcome::Trailed { new_stop } => {
                assert!(approx_eq_frac(new_stop, low + abs_off));
            }
            o => panic!("expected Trailed, got {:?}", o),
        }

        let new_stop = ts.stop_price().unwrap();
        // Slight unfavorable bounce: stay strictly below stop (short breach is price >= stop).
        let pullback = (new_stop + low) * 0.5;
        assert!(pullback > low && pullback < new_stop);
        assert_eq!(ts.on_price(pullback), TickOutcome::NoOp);

        let breach = new_stop + 0.01;
        assert!(breach < 0.99);
        match ts.on_price(breach) {
            TickOutcome::Triggered { stop_price, .. } => {
                assert!(approx_eq_frac(stop_price, new_stop));
            }
            o => panic!("expected Triggered, got {:?}", o),
        }
    }

    #[test]
    fn frac_long_activation_by_price() {
        let entry = 0.20;
        let arm_at = 0.45;
        let pct = 0.01;
        let ts = TrailingStop::new(
            Side::Long,
            entry,
            TrailSpec::Percent(pct),
            Activation::Price(arm_at),
        );
        assert!(!ts.is_armed());
        assert_eq!(ts.on_price(0.30), TickOutcome::NoOp);
        assert_eq!(ts.on_price(0.44), TickOutcome::NoOp);

        let cross = 0.46;
        match ts.on_price(cross) {
            TickOutcome::Armed { stop_price } => {
                let want = want_stop_at(Side::Long, TrailSpec::Percent(pct), cross);
                assert!(approx_eq_frac(stop_price, want));
            }
            o => panic!("expected Armed, got {:?}", o),
        }
        assert!(ts.is_armed());
    }

    #[test]
    fn frac_short_activation_by_price() {
        let entry = 0.80;
        let arm_at = 0.35;
        let pct = 0.015;
        let ts = TrailingStop::new(
            Side::Short,
            entry,
            TrailSpec::Percent(pct),
            Activation::Price(arm_at),
        );
        assert_eq!(ts.on_price(0.50), TickOutcome::NoOp);
        match ts.on_price(0.34) {
            TickOutcome::Armed { stop_price } => {
                let want = want_stop_at(Side::Short, TrailSpec::Percent(pct), 0.34);
                assert!(approx_eq_frac(stop_price, want));
            }
            o => panic!("expected Armed, got {:?}", o),
        }
    }

    #[test]
    fn frac_long_activation_favorable_distance() {
        let entry = 0.35;
        let need = 0.10;
        let ts = TrailingStop::new(
            Side::Long,
            entry,
            TrailSpec::Percent(0.02),
            Activation::FavorableDistance(need),
        );
        assert_eq!(ts.on_price(0.40), TickOutcome::NoOp);
        // `entry + need` is not always representable; arm clearly past the threshold.
        let arm_price = 0.46;
        assert!(arm_price - entry > need - 1e-12);
        match ts.on_price(arm_price) {
            TickOutcome::Armed { stop_price } => {
                let want = want_stop_at(Side::Long, TrailSpec::Percent(0.02), arm_price);
                assert!(approx_eq_frac(stop_price, want));
            }
            o => panic!("expected Armed, got {:?}", o),
        }
    }

    #[test]
    fn frac_short_activation_favorable_distance() {
        let entry = 0.72;
        let need = 0.08;
        let ts = TrailingStop::new(
            Side::Short,
            entry,
            TrailSpec::Absolute(0.015),
            Activation::FavorableDistance(need),
        );
        assert_eq!(ts.on_price(0.68), TickOutcome::NoOp);
        let arm_price = 0.63;
        assert!(entry - arm_price > need - 1e-12);
        match ts.on_price(arm_price) {
            TickOutcome::Armed { stop_price } => {
                let want = want_stop_at(Side::Short, TrailSpec::Absolute(0.015), arm_price);
                assert!(approx_eq_frac(stop_price, want));
            }
            o => panic!("expected Armed, got {:?}", o),
        }
    }

    #[test]
    fn frac_long_gap_through_stop_on_first_tick() {
        let entry = 0.60;
        let abs = 0.05;
        let ts = TrailingStop::new(
            Side::Long,
            entry,
            TrailSpec::Absolute(abs),
            Activation::Immediate,
        );
        let want_stop = entry - abs;
        match ts.on_price(0.02) {
            TickOutcome::Triggered {
                trigger_price,
                stop_price,
            } => {
                assert!(approx_eq_frac(trigger_price, 0.02));
                assert!(approx_eq_frac(stop_price, want_stop));
            }
            o => panic!("expected Triggered, got {:?}", o),
        }
    }

    #[test]
    fn frac_short_gap_through_stop_on_first_tick() {
        let entry = 0.15;
        let abs = 0.03;
        let ts = TrailingStop::new(
            Side::Short,
            entry,
            TrailSpec::Absolute(abs),
            Activation::Immediate,
        );
        let want_stop = entry + abs;
        match ts.on_price(0.97) {
            TickOutcome::Triggered {
                trigger_price,
                stop_price,
            } => {
                assert!(approx_eq_frac(trigger_price, 0.97));
                assert!(approx_eq_frac(stop_price, want_stop));
            }
            o => panic!("expected Triggered, got {:?}", o),
        }
    }

    #[test]
    fn frac_long_trigger_exactly_at_stop() {
        let ts = TrailingStop::new(
            Side::Long,
            0.40,
            TrailSpec::Percent(0.05),
            Activation::Immediate,
        );
        let stop = ts.stop_price().unwrap();
        match ts.on_price(stop) {
            TickOutcome::Triggered { .. } => {}
            o => panic!("expected Triggered at stop, got {:?}", o),
        }
    }

    #[test]
    fn frac_short_trigger_exactly_at_stop() {
        let ts = TrailingStop::new(
            Side::Short,
            0.88,
            TrailSpec::Absolute(0.04),
            Activation::Immediate,
        );
        let stop = ts.stop_price().unwrap();
        match ts.on_price(stop) {
            TickOutcome::Triggered { .. } => {}
            o => panic!("expected Triggered at stop, got {:?}", o),
        }
    }

    #[test]
    fn frac_boundary_entry_0_01_percent_trail() {
        let entry = 0.01;
        let pct = 0.10;
        let ts = TrailingStop::new(
            Side::Long,
            entry,
            TrailSpec::Percent(pct),
            Activation::Immediate,
        );
        let stop0 = want_stop_at(Side::Long, TrailSpec::Percent(pct), entry);
        assert!(approx_eq_frac(ts.stop_price().unwrap(), stop0));

        match ts.on_price(0.99) {
            TickOutcome::Trailed { new_stop } => {
                let want = want_stop_at(Side::Long, TrailSpec::Percent(pct), 0.99);
                assert!(approx_eq_frac(new_stop, want));
            }
            o => panic!("expected Trailed, got {:?}", o),
        }
        assert!(ts.best_price().unwrap() <= 0.99);
    }

    #[test]
    fn frac_long_percent_trail_uses_full_tick_distance_sweep() {
        for pct in [0.01, 0.02, 0.05, 0.10] {
            for best_tick in 1..=99 {
                let price = from_tick(best_tick);
                let spec = TrailSpec::Percent(pct);
                let stop_tick = TrailingStop::stop_tick_for_best(Side::Long, &spec, best_tick);
                let want_distance_ticks = ((best_tick as f64 * pct).ceil() as i64).max(1);

                assert_eq!(
                    best_tick - stop_tick,
                    want_distance_ticks,
                    "long pct={pct} price={price:.2} stop={:.2}",
                    from_tick(stop_tick)
                );

                let ts = TrailingStop::new(Side::Long, price, spec, Activation::Immediate);
                assert_eq!(
                    ts.on_price(price),
                    TickOutcome::NoOp,
                    "long pct={pct} should not trigger on unchanged price {price:.2}"
                );
            }
        }
    }

    #[test]
    fn frac_short_percent_trail_uses_full_tick_distance_sweep() {
        for pct in [0.01, 0.02, 0.05, 0.10] {
            for best_tick in 1..=99 {
                let price = from_tick(best_tick);
                let spec = TrailSpec::Percent(pct);
                let stop_tick = TrailingStop::stop_tick_for_best(Side::Short, &spec, best_tick);
                let want_distance_ticks = ((best_tick as f64 * pct).ceil() as i64).max(1);

                assert_eq!(
                    stop_tick - best_tick,
                    want_distance_ticks,
                    "short pct={pct} price={price:.2} stop={:.2}",
                    from_tick(stop_tick)
                );

                let ts = TrailingStop::new(Side::Short, price, spec, Activation::Immediate);
                assert_eq!(
                    ts.on_price(price),
                    TickOutcome::NoOp,
                    "short pct={pct} should not trigger on unchanged price {price:.2}"
                );
            }
        }
    }

    #[test]
    fn frac_long_absolute_trail_uses_full_tick_distance_sweep() {
        for offset in [0.005, 0.01, 0.015, 0.02] {
            for best_tick in 1..=99 {
                let price = from_tick(best_tick);
                let spec = TrailSpec::Absolute(offset);
                let stop_tick = TrailingStop::stop_tick_for_best(Side::Long, &spec, best_tick);
                let want_distance_ticks = ((offset / TICK).ceil() as i64).max(1);

                assert_eq!(
                    best_tick - stop_tick,
                    want_distance_ticks,
                    "long offset={offset} price={price:.2} stop={:.2}",
                    from_tick(stop_tick)
                );

                let ts = TrailingStop::new(Side::Long, price, spec, Activation::Immediate);
                assert_eq!(
                    ts.on_price(price),
                    TickOutcome::NoOp,
                    "long offset={offset} should not trigger on unchanged price {price:.2}"
                );
            }
        }
    }

    #[test]
    fn frac_short_absolute_trail_uses_full_tick_distance_sweep() {
        for offset in [0.005, 0.01, 0.015, 0.02] {
            for best_tick in 1..=99 {
                let price = from_tick(best_tick);
                let spec = TrailSpec::Absolute(offset);
                let stop_tick = TrailingStop::stop_tick_for_best(Side::Short, &spec, best_tick);
                let want_distance_ticks = ((offset / TICK).ceil() as i64).max(1);

                assert_eq!(
                    stop_tick - best_tick,
                    want_distance_ticks,
                    "short offset={offset} price={price:.2} stop={:.2}",
                    from_tick(stop_tick)
                );

                let ts = TrailingStop::new(Side::Short, price, spec, Activation::Immediate);
                assert_eq!(
                    ts.on_price(price),
                    TickOutcome::NoOp,
                    "short offset={offset} should not trigger on unchanged price {price:.2}"
                );
            }
        }
    }

    #[test]
    fn frac_many_small_ratchets_monotonic_stop() {
        let ts = TrailingStop::new(
            Side::Long,
            0.25,
            TrailSpec::Percent(0.01),
            Activation::Immediate,
        );
        let mut last_stop = ts.stop_price().unwrap();
        let spec = TrailSpec::Percent(0.01);
        for p in [0.26, 0.27, 0.28, 0.29, 0.30, 0.31] {
            match ts.on_price(p) {
                TickOutcome::Trailed { new_stop } => {
                    assert!(new_stop > last_stop, "stop should ratchet up");
                    let want = want_stop_at(Side::Long, spec, p);
                    assert!(approx_eq_frac(new_stop, want));
                    last_stop = new_stop;
                }
                TickOutcome::NoOp => panic!("expected Trailed at {}", p),
                o => panic!("unexpected {:?} at {}", o, p),
            }
        }
    }

    #[test]
    fn frac_short_many_small_ratchets_monotonic_stop() {
        let ts = TrailingStop::new(
            Side::Short,
            0.75,
            TrailSpec::Percent(0.01),
            Activation::Immediate,
        );
        let mut last_stop = ts.stop_price().unwrap();
        let spec = TrailSpec::Percent(0.01);
        for p in [0.74, 0.73, 0.72, 0.71, 0.70] {
            match ts.on_price(p) {
                TickOutcome::Trailed { new_stop } => {
                    assert!(new_stop < last_stop, "short stop should ratchet down");
                    let want = want_stop_at(Side::Short, spec, p);
                    assert!(approx_eq_frac(new_stop, want));
                    last_stop = new_stop;
                }
                TickOutcome::NoOp => panic!("expected Trailed at {}", p),
                o => panic!("unexpected {:?} at {}", o, p),
            }
        }
    }
}
