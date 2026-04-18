//! Central application state + event dispatch.
//!
//! Three async sources push into a single `AppEvent` channel:
//!   1. crossterm key events
//!   2. Chainlink price ticks
//!   3. CLOB book snapshots
//!   4. CLOB conditional balances (positions) after each market roll
//!   5. Periodic ticks for market rolling
//!
//! Trading actions post results back through the same channel.

use chrono::{DateTime, Utc};
use std::collections::VecDeque;

use crate::feeds::{chainlink::PriceTick, clob_ws::BookSnapshot};
use crate::gamma::ActiveMarket;
use crate::trading::{ClobTrade, Side, OrderType};
use tracing::debug;

// ── Public event enum ───────────────────────────────────────────────

#[derive(Debug)]
pub enum AppEvent {
    Tick,                         // 1-Hz clock
    Price(PriceTick),
    Book(BookSnapshot),
    MarketRoll(ActiveMarket),
    /// CLOB positions for the current market: balances + cost basis replayed from `GET /data/trades`.
    PositionsLoaded {
        position_up:   Position,
        position_down: Position,
        /// Newest first, capped before send — merged into `fills` (session panel).
        fills_bootstrap: Vec<Fill>,
    },
    Key(crossterm::event::KeyEvent),
    OrderAck { side: Side, outcome: Outcome, qty: f64, price: f64 },
    OrderErr(String),
}

// ── UI-level types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome { Up, Down }
impl Outcome {
    pub fn as_str(self) -> &'static str { match self { Outcome::Up => "UP", Outcome::Down => "DOWN" } }
    pub fn opposite(self) -> Self { match self { Outcome::Up => Outcome::Down, Outcome::Down => Outcome::Up } }
}

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub shares:    f64,
    pub avg_entry: f64, // volume-weighted, in price terms (0.01..0.99)
}
impl Position {
    fn add(&mut self, qty: f64, price: f64) {
        let new_total = self.shares + qty;
        if new_total.abs() < 1e-9 { *self = Default::default(); return; }
        self.avg_entry = (self.shares * self.avg_entry + qty * price) / new_total;
        self.shares    = new_total;
    }
    fn reduce(&mut self, qty: f64, price: f64) -> f64 {
        // returns realized pnl for the portion closed
        let closed = qty.min(self.shares);
        let pnl    = closed * (price - self.avg_entry);
        self.shares -= closed;
        if self.shares.abs() < 1e-9 { *self = Default::default(); }
        pnl
    }

    /// One CLOB fill: BUY extends inventory; SELL realizes against `avg_entry`.
    pub fn apply_fill(&mut self, side: Side, qty: f64, price: f64) -> f64 {
        match side {
            Side::Buy => {
                self.add(qty, price);
                0.0
            }
            Side::Sell => self.reduce(qty, price),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Fill {
    pub ts:      DateTime<Utc>,
    pub side:    Side,
    pub outcome: Outcome,
    pub qty:     f64,
    pub price:   f64,
    pub realized: f64, // only non-zero when the fill closes part of a position
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    EditSize,
    LimitModal { outcome: Outcome, side: Side, field: LimitField },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitField { Price, Size }

#[derive(Debug, Clone)]
pub struct AppState {
    pub market:         Option<ActiveMarket>,
    pub btc_price:      Option<f64>,
    pub btc_price_ts:   Option<DateTime<Utc>>,

    // One book per outcome token
    pub book_up:        Option<BookSnapshot>,
    pub book_down:      Option<BookSnapshot>,

    pub position_up:    Position,
    pub position_down:  Position,
    pub realized_pnl:   f64,

    pub fills:          VecDeque<Fill>,
    pub status_line:    String,

    /// When Gamma omits the opening USD level in the market text, we latch the
    /// Chainlink oracle reading: first tick whose `payload.timestamp` is at or
    /// after [`ActiveMarket::opens_at`] (5m window boundary). Matches Polymarket
    /// RTDS docs + community notes (same oracle stream as resolution).
    latched_price_to_beat: Option<f64>,

    pub default_size_usdc: f64,
    pub size_input:        String, // buffer while editing size
    pub limit_price_input: String,
    pub limit_size_input:  String,

    pub input_mode:        InputMode,
}

impl AppState {
    pub fn new(default_size_usdc: f64) -> Self {
        Self {
            market: None, btc_price: None, btc_price_ts: None,
            book_up: None, book_down: None,
            position_up: Default::default(), position_down: Default::default(),
            realized_pnl: 0.0, fills: VecDeque::with_capacity(64),
            status_line: "Waiting for market data…".into(),
            latched_price_to_beat: None,
            default_size_usdc,
            size_input: format!("{default_size_usdc:.2}"),
            limit_price_input: String::new(),
            limit_size_input:  String::new(),
            input_mode: InputMode::Normal,
        }
    }

    // ── Queries ─────────────────────────────────────────────────────
    pub fn price_to_beat(&self) -> Option<f64> {
        self.market
            .as_ref()
            .and_then(|m| m.price_to_beat)
            .or(self.latched_price_to_beat)
    }

    /// BTC colour: green when we're at/above the opening price (UP winning),
    /// red when strictly below.
    pub fn btc_above_target(&self) -> Option<bool> {
        match (self.btc_price, self.price_to_beat()) {
            (Some(p), Some(t)) => Some(p >= t),
            _ => None,
        }
    }

    pub fn book_for(&self, outcome: Outcome) -> Option<&BookSnapshot> {
        match outcome { Outcome::Up => self.book_up.as_ref(), Outcome::Down => self.book_down.as_ref() }
    }

    /// Best ask on the outcome side we'd hit when buying YES(outcome).
    pub fn best_ask(&self, outcome: Outcome) -> Option<f64> {
        self.book_for(outcome)?.asks.first().map(|l| l.price)
    }
    /// Best bid on the outcome side we'd hit when selling YES(outcome).
    pub fn best_bid(&self, outcome: Outcome) -> Option<f64> {
        self.book_for(outcome)?.bids.first().map(|l| l.price)
    }

    /// Current mark (mid) price for an outcome, for unrealized PnL.
    pub fn mark(&self, outcome: Outcome) -> Option<f64> {
        let b = self.book_for(outcome)?;
        let bid = b.bids.first().map(|l| l.price);
        let ask = b.asks.first().map(|l| l.price);
        match (bid, ask) {
            (Some(b), Some(a)) => Some((b + a) / 2.0),
            (Some(b), None)    => Some(b),
            (None, Some(a))    => Some(a),
            _                  => None,
        }
    }

    pub fn position(&self, outcome: Outcome) -> &Position {
        match outcome { Outcome::Up => &self.position_up, Outcome::Down => &self.position_down }
    }
    fn position_mut(&mut self, outcome: Outcome) -> &mut Position {
        match outcome { Outcome::Up => &mut self.position_up, Outcome::Down => &mut self.position_down }
    }

    pub fn unrealized_pnl(&self, outcome: Outcome) -> f64 {
        let p = self.position(outcome);
        match self.mark(outcome) {
            Some(m) if p.shares > 0.0 => p.shares * (m - p.avg_entry),
            _ => 0.0,
        }
    }

    pub fn total_pnl(&self) -> f64 {
        self.realized_pnl + self.unrealized_pnl(Outcome::Up) + self.unrealized_pnl(Outcome::Down)
    }

    pub fn countdown_secs(&self) -> Option<i64> {
        self.market.as_ref().map(|m| (m.closes_at - Utc::now()).num_seconds().max(0))
    }

    pub fn current_size(&self) -> f64 {
        self.size_input.parse().unwrap_or(self.default_size_usdc)
    }

    // ── Mutations ───────────────────────────────────────────────────
    pub fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Tick => {}
            AppEvent::Price(p) => {
                self.btc_price    = Some(p.price);
                self.btc_price_ts = chrono::DateTime::<Utc>::from_timestamp_millis(p.timestamp_ms as i64);
                if let Some(m) = &self.market {
                    if m.price_to_beat.is_none() && self.latched_price_to_beat.is_none() {
                        let open_ms = m.opens_at.timestamp_millis().max(0) as u64;
                        // Require oracle time ≥ window open (per RTDS payload timestamps).
                        if p.timestamp_ms >= open_ms {
                            self.latched_price_to_beat = Some(p.price);
                        }
                    }
                }
            }
            AppEvent::Book(b) => {
                if let Some(m) = &self.market {
                    if b.asset_id == m.up_token_id   { self.book_up   = Some(b); }
                    else if b.asset_id == m.down_token_id { self.book_down = Some(b); }
                }
            }
            AppEvent::MarketRoll(m) => {
                // Close any positions from the previous market — they'll resolve
                // via Polymarket and show up as realized once winnings redeem.
                // We keep realized_pnl but zero out live positions for the new market.
                // Return to normal keys (u/d/…) — otherwise `s` / limit modal survives a roll.
                self.input_mode = InputMode::Normal;
                self.status_line = format!("New market: {}", m.question);
                self.latched_price_to_beat = None;
                self.market = Some(m);
                self.book_up = None; self.book_down = None;
                self.position_up = Default::default();
                self.position_down = Default::default();
            }
            AppEvent::PositionsLoaded {
                position_up,
                position_down,
                fills_bootstrap,
            } => {
                self.position_up = position_up;
                self.position_down = position_down;
                self.fills.clear();
                for f in fills_bootstrap {
                    self.fills.push_front(f);
                    if self.fills.len() > 64 {
                        self.fills.pop_back();
                    }
                }
                self.status_line = format!(
                    "Positions from CLOB — UP {:.2} @ {:.3} / DOWN {:.2} @ {:.3}",
                    self.position_up.shares,
                    self.position_up.avg_entry,
                    self.position_down.shares,
                    self.position_down.avg_entry,
                );
            }
            AppEvent::OrderAck { side, outcome, qty, price } => {
                let realized = self.position_mut(outcome).apply_fill(side, qty, price);
                self.realized_pnl += realized;
                self.fills.push_front(Fill {
                    ts: Utc::now(), side, outcome, qty, price, realized,
                });
                if self.fills.len() > 64 { self.fills.pop_back(); }
                self.status_line = format!("{} {qty:.2} {} @ {price:.3} ✓",
                    side_str(side), outcome.as_str());
            }
            AppEvent::OrderErr(e) => self.status_line = format!("✗ {e}"),
            AppEvent::Key(_) => {} // handled in main via `events::handle_key`
        }
    }
}

fn side_str(s: Side) -> &'static str { match s { Side::Buy => "BUY", Side::Sell => "SELL" } }

fn parse_trade_timestamp(s: &str) -> DateTime<Utc> {
    let s = s.trim();
    if s.is_empty() {
        return DateTime::<Utc>::UNIX_EPOCH;
    }
    if let Ok(n) = s.parse::<i64>() {
        if n > 100_000_000_000 {
            return DateTime::from_timestamp_millis(n).unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
        }
        return DateTime::from_timestamp(n, 0).unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    }
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

fn parse_clob_side(s: &str) -> Option<Side> {
    match s.trim().to_ascii_uppercase().as_str() {
        "BUY" => Some(Side::Buy),
        "SELL" => Some(Side::Sell),
        _ => None,
    }
}

fn merge_chain_balance(replay: Position, balance: f64) -> Position {
    if !balance.is_finite() || balance.abs() < 1e-12 {
        return Position::default();
    }
    let shares = balance;
    let tol = f64::max(0.02, 0.02 * balance.abs());
    let avg_entry = if replay.shares > 1e-9 {
        if (replay.shares - balance).abs() > tol {
            debug!(
                replay_shares = replay.shares,
                chain_shares = balance,
                "position replay vs chain balance mismatch; keeping replay avg as estimate"
            );
        }
        replay.avg_entry
    } else {
        0.0
    };
    Position {
        shares,
        avg_entry,
    }
}

/// Replay Polymarket `GET /data/trades` for the active market to recover VWAP entries and fills.
pub fn hydrate_positions_from_trades(
    trades: &[ClobTrade],
    up_token_id: &str,
    down_token_id: &str,
    balance_up: f64,
    balance_down: f64,
) -> (Position, Position, Vec<Fill>) {
    let mut indexed: Vec<(DateTime<Utc>, &str, &ClobTrade)> = trades
        .iter()
        .filter(|t| t.asset_id == up_token_id || t.asset_id == down_token_id)
        .map(|t| (parse_trade_timestamp(&t.match_time), t.id.as_str(), t))
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));

    let mut up = Position::default();
    let mut down = Position::default();
    let mut fills_chrono: Vec<Fill> = Vec::new();

    for (_, _, t) in indexed {
        let outcome = if t.asset_id == up_token_id {
            Outcome::Up
        } else {
            Outcome::Down
        };
        let Some(side) = parse_clob_side(&t.side) else {
            debug!(id = %t.id, side = %t.side, "skip trade with unknown side");
            continue;
        };
        let Ok(qty) = t.size.parse::<f64>() else {
            debug!(id = %t.id, "skip trade with bad size");
            continue;
        };
        let Ok(price) = t.price.parse::<f64>() else {
            debug!(id = %t.id, "skip trade with bad price");
            continue;
        };
        if !qty.is_finite() || qty <= 0.0 || !price.is_finite() {
            continue;
        }
        let ts = parse_trade_timestamp(&t.match_time);
        let realized = match outcome {
            Outcome::Up => up.apply_fill(side, qty, price),
            Outcome::Down => down.apply_fill(side, qty, price),
        };
        fills_chrono.push(Fill {
            ts,
            side,
            outcome,
            qty,
            price,
            realized,
        });
    }

    let position_up = merge_chain_balance(up, balance_up);
    let position_down = merge_chain_balance(down, balance_down);

    let fills_bootstrap: Vec<Fill> = fills_chrono.into_iter().rev().take(64).collect();

    (position_up, position_down, fills_bootstrap)
}

fn clamp_prob(p: f64) -> f64 {
    p.clamp(0.01, 0.99)
}

/// Derive the OrderArgs + order_type we'd submit for a given user intent,
/// given the current book.
pub fn resolve_market_order(
    state: &AppState,
    outcome: Outcome,
    side: Side,
    size: f64,
    slippage_bps: u32,
) -> Option<(f64, f64, OrderType)> {
    let slip = slippage_bps as f64 / 10_000.0;
    match side {
        Side::Buy => {
            let ask = state.best_ask(outcome)?;
            // Widen ceiling so a thin or stale book still crosses (`MARKET_SLIPPAGE_BPS`).
            let price = clamp_prob(ask * (1.0 + slip));
            // `size` = USDC notional → shares at reference ask
            let shares = (size / ask).max(0.01);
            Some((shares, price, OrderType::Fak))
        }
        Side::Sell => {
            let bid = state.best_bid(outcome)?;
            let price = clamp_prob(bid * (1.0 - slip));
            // `size` = shares to sell (maker leg is outcome tokens; must not treat as USDC).
            let shares = size.max(0.01);
            Some((shares, price, OrderType::Fak))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::ClobTrade;

    fn trade(
        id: &str,
        asset_id: &str,
        side: &str,
        size: &str,
        price: &str,
        match_time: &str,
    ) -> ClobTrade {
        ClobTrade {
            id: id.to_string(),
            asset_id: asset_id.to_string(),
            side: side.to_string(),
            size: size.to_string(),
            price: price.to_string(),
            match_time: match_time.to_string(),
        }
    }

    #[test]
    fn hydrate_vwap_two_buys() {
        let up = "111";
        let down = "222";
        let trades = vec![
            trade("a", up, "BUY", "10", "0.5", "1000"),
            trade("b", up, "BUY", "10", "0.6", "2000"),
        ];
        let (pu, pd, fills) = hydrate_positions_from_trades(&trades, up, down, 20.0, 0.0);
        assert!((pu.shares - 20.0).abs() < 1e-6);
        assert!((pu.avg_entry - 0.55).abs() < 1e-6);
        assert!(pd.shares.abs() < 1e-9);
        assert_eq!(fills.len(), 2);
    }

    #[test]
    fn hydrate_sell_realized_in_fill() {
        let up = "111";
        let trades = vec![
            trade("a", up, "BUY", "10", "0.5", "1000"),
            trade("b", up, "SELL", "4", "0.7", "2000"),
        ];
        let (pu, _, fills) = hydrate_positions_from_trades(&trades, up, "222", 6.0, 0.0);
        assert!((pu.shares - 6.0).abs() < 1e-6);
        assert!((pu.avg_entry - 0.5).abs() < 1e-6);
        let sell_fill = fills.iter().find(|f| f.side == Side::Sell).unwrap();
        assert!((sell_fill.realized - 4.0 * (0.7 - 0.5)).abs() < 1e-6);
    }
}
