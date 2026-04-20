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
use crate::fees::polymarket_crypto_taker_fee_usdc;
use crate::gamma::ActiveMarket;
use crate::trading::{clob_asset_ids_match, ClobOpenOrder, ClobTrade, OrderType, Side};
use tracing::debug;

/// Minimum outcome shares for a limit (GTD) order — enforced in the UI before submit.
pub const MIN_LIMIT_ORDER_SHARES: f64 = 5.0;

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
    /// Resting orders for the active market (`GET /data/orders`).
    OpenOrdersLoaded { orders: Vec<OpenOrderRow> },
    /// USDC.e cash + claimable from on-chain reads (Multicall3); neg-risk redeemable sums still use Data API.
    BalancePanelLoaded { cash_usdc: f64, claimable_usdc: f64 },
    Key(crossterm::event::KeyEvent),
    OrderAck { side: Side, outcome: Outcome, qty: f64, price: f64 },
    /// Non-blocking status line only (no modal).
    OrderErr(String),
    /// Order placement / cancel failures: centered modal + status line (Enter dismisses).
    OrderErrModal(String),
    /// Status line update without the `✗` prefix (e.g. claim hint).
    StatusInfo(String),
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
    /// Volume-weighted average **USDC cost per share**, including Polymarket **taker** fees on buys
    /// (Crypto: `fee = C × 0.072 × p × (1-p)` per [fees](https://docs.polymarket.com/trading/fees)).
    pub avg_entry: f64,
}
impl Position {
    fn add(&mut self, qty: f64, price: f64) {
        let new_total = self.shares + qty;
        if new_total.abs() < 1e-9 { *self = Default::default(); return; }
        let buy_cost =
            qty.mul_add(price, polymarket_crypto_taker_fee_usdc(qty, price));
        self.avg_entry = (self.shares * self.avg_entry + buy_cost) / new_total;
        self.shares    = new_total;
    }
    fn reduce(&mut self, qty: f64, price: f64) -> f64 {
        // returns realized pnl for the portion closed (taker fee on sell deducted)
        let closed = qty.min(self.shares);
        let fee_out = polymarket_crypto_taker_fee_usdc(closed, price);
        let pnl = closed.mul_add(price, -fee_out) - closed * self.avg_entry;
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

/// One row in the Open orders panel (current market only).
#[derive(Debug, Clone)]
pub struct OpenOrderRow {
    pub side:    Side,
    pub outcome: Outcome,
    pub price:   f64,
    /// Unfilled size (shares).
    pub remaining: f64,
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
    /// Resting limit orders on the active market (from CLOB).
    pub open_orders:    Vec<OpenOrderRow>,
    pub status_line:    String,

    /// USDC.e (`0x2791…174`) `balanceOf(funder)` on Polygon (via Multicall3).
    pub collateral_cash_usdc: Option<f64>,
    /// Resolved CTF claimable USDC from `payoutNumerators`/`payoutDenominator` + ERC-1155 balances; neg-risk from Data API.
    pub collateral_claimable_usdc: Option<f64>,

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

    /// When set, a centered error dialog is drawn on top; Enter clears it.
    pub error_dialog:      Option<String>,
}

impl AppState {
    pub fn new(default_size_usdc: f64) -> Self {
        Self {
            market: None, btc_price: None, btc_price_ts: None,
            book_up: None, book_down: None,
            position_up: Default::default(), position_down: Default::default(),
            realized_pnl: 0.0, fills: VecDeque::with_capacity(64),
            open_orders: Vec::new(),
            status_line: "Waiting for market data…".into(),
            collateral_cash_usdc: None,
            collateral_claimable_usdc: None,
            latched_price_to_beat: None,
            default_size_usdc,
            size_input: format!("{default_size_usdc:.2}"),
            limit_price_input: String::new(),
            limit_size_input:  String::new(),
            input_mode: InputMode::Normal,
            error_dialog: None,
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
            Some(m) if p.shares > 0.0 => {
                let fee_out = polymarket_crypto_taker_fee_usdc(p.shares, m);
                p.shares.mul_add(m, -fee_out) - p.shares * p.avg_entry
            }
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
                self.error_dialog = None;
                self.status_line = format!("New market: {}", m.question);
                self.latched_price_to_beat = None;
                self.market = Some(m);
                self.book_up = None; self.book_down = None;
                self.position_up = Default::default();
                self.position_down = Default::default();
                self.open_orders.clear();
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
            AppEvent::OpenOrdersLoaded { orders } => {
                self.open_orders = orders;
            }
            AppEvent::BalancePanelLoaded { cash_usdc, claimable_usdc } => {
                self.collateral_cash_usdc = Some(cash_usdc);
                self.collateral_claimable_usdc = Some(claimable_usdc);
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
            AppEvent::OrderErrModal(e) => {
                self.status_line = format!("✗ {e}");
                self.error_dialog = Some(e);
            },
            AppEvent::StatusInfo(msg) => self.status_line = msg,
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

/// Unfilled size of resting **SELL** orders per outcome (shares escrowed off the spendable balance).
pub fn escrow_sell_shares_from_clob_orders(
    rows: &[ClobOpenOrder],
    up_token_id: &str,
    down_token_id: &str,
) -> (f64, f64) {
    let mut up = 0.0f64;
    let mut down = 0.0f64;
    for o in rows {
        let outcome = if clob_asset_ids_match(&o.asset_id, up_token_id) {
            Some(Outcome::Up)
        } else if clob_asset_ids_match(&o.asset_id, down_token_id) {
            Some(Outcome::Down)
        } else {
            continue;
        };
        let Some(side) = parse_clob_side(&o.side) else { continue };
        if side != Side::Sell {
            continue;
        }
        let orig = o.original_size.parse::<f64>().unwrap_or(f64::NAN);
        let matched = o.size_matched.parse::<f64>().unwrap_or(0.0);
        let remaining = orig - matched;
        if !remaining.is_finite() || remaining <= 1e-9 {
            continue;
        }
        match outcome {
            Some(Outcome::Up) => up += remaining,
            Some(Outcome::Down) => down += remaining,
            None => {}
        }
    }
    (up, down)
}

/// Map `GET /data/orders` rows to UI rows for the UP/DOWN token pair.
pub fn open_orders_from_clob(
    rows: Vec<ClobOpenOrder>,
    up_token_id: &str,
    down_token_id: &str,
) -> Vec<OpenOrderRow> {
    let mut out = Vec::new();
    for o in rows {
        let outcome = if clob_asset_ids_match(&o.asset_id, up_token_id) {
            Outcome::Up
        } else if clob_asset_ids_match(&o.asset_id, down_token_id) {
            Outcome::Down
        } else {
            continue;
        };
        let Some(side) = parse_clob_side(&o.side) else { continue };
        let orig = o.original_size.parse::<f64>().unwrap_or(f64::NAN);
        let matched = o.size_matched.parse::<f64>().unwrap_or(0.0);
        let price = o.price.parse::<f64>().unwrap_or(f64::NAN);
        let remaining = orig - matched;
        if !remaining.is_finite() || remaining <= 1e-9 {
            continue;
        }
        if !price.is_finite() || price <= 0.0 {
            continue;
        }
        out.push(OpenOrderRow {
            side,
            outcome,
            price,
            remaining,
        });
    }
    out.sort_by(|a, b| {
        let ord_o = match (a.outcome, b.outcome) {
            (Outcome::Up, Outcome::Up) | (Outcome::Down, Outcome::Down) => std::cmp::Ordering::Equal,
            (Outcome::Up, Outcome::Down) => std::cmp::Ordering::Less,
            (Outcome::Down, Outcome::Up) => std::cmp::Ordering::Greater,
        };
        ord_o.then_with(|| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal))
    });
    out
}

/// Combine trade replay with on-chain conditional balance.
///
/// Polymarket `GET /balance-allowance` for `CONDITIONAL` returns **spendable** outcome shares.
/// Tokens committed to a resting **SELL** disappear from that balance until the order fills or
/// cancels, while [`GET /data/trades`](https://docs.polymarket.com) still reflects true net
/// position. The Positions panel therefore prefers replay for `shares` (and VWAP) whenever we
/// have it, then `balance + escrow_sell`, then [`Data API`](crate::data_api) `size` / `avgPrice`
/// when the trade list is empty, failed to load, or `asset_id` strings did not match.
fn merge_chain_balance(
    replay: Position,
    balance: f64,
    escrow_sell: f64,
    data_api: Option<(f64, f64)>,
) -> Position {
    let spendable = if balance.is_finite() { balance.max(0.0) } else { 0.0 };
    let escrow = if escrow_sell.is_finite() && escrow_sell > 0.0 {
        escrow_sell
    } else {
        0.0
    };
    let inventory_fallback = spendable + escrow;
    let bal_ok = spendable >= 1e-12;
    let escrow_ok = escrow >= 1e-12;
    let replay_ok = replay.shares > 1e-9;

    let data_shares = data_api
        .map(|(s, _)| s)
        .filter(|s| s.is_finite() && *s > 1e-12)
        .unwrap_or(0.0);
    let data_avg = data_api.and_then(|(s, a)| {
        (s.is_finite() && s > 1e-12 && a.is_finite() && a > 0.0).then_some(a)
    });

    if !replay_ok && !bal_ok && !escrow_ok && data_shares < 1e-12 {
        return Position::default();
    }

    let shares = if replay_ok {
        replay.shares
    } else if inventory_fallback > 1e-12 {
        inventory_fallback
    } else if data_shares > 1e-12 {
        data_shares
    } else {
        0.0
    };

    let avg_entry = if replay_ok {
        let tol = f64::max(
            0.02,
            0.02 * f64::max(replay.shares.abs(), inventory_fallback.max(data_shares)),
        );
        if (bal_ok || escrow_ok) && (replay.shares - inventory_fallback).abs() > tol {
            debug!(
                replay_shares = replay.shares,
                spendable_balance_shares = spendable,
                escrow_sell_shares = escrow,
                "position from trades vs spendable + SELL escrow"
            );
        }
        replay.avg_entry
    } else if data_shares > 1e-12
        && (shares - data_shares).abs() <= f64::max(1e-6, 0.02 * shares.max(data_shares))
    {
        data_avg.unwrap_or(0.0)
    } else {
        0.0
    };

    Position { shares, avg_entry }
}

/// Replay Polymarket `GET /data/trades` for the active market to recover VWAP entries and fills.
pub fn hydrate_positions_from_trades(
    trades: &[ClobTrade],
    up_token_id: &str,
    down_token_id: &str,
    balance_up: f64,
    balance_down: f64,
    escrow_sell_up: f64,
    escrow_sell_down: f64,
    data_api_up: Option<(f64, f64)>,
    data_api_down: Option<(f64, f64)>,
) -> (Position, Position, Vec<Fill>) {
    let mut indexed: Vec<(DateTime<Utc>, &str, &ClobTrade)> = trades
        .iter()
        .filter(|t| {
            clob_asset_ids_match(&t.asset_id, up_token_id)
                || clob_asset_ids_match(&t.asset_id, down_token_id)
        })
        .map(|t| (parse_trade_timestamp(&t.match_time), t.id.as_str(), t))
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));

    let mut up = Position::default();
    let mut down = Position::default();
    let mut fills_chrono: Vec<Fill> = Vec::new();

    for (_, _, t) in indexed {
        let outcome = if clob_asset_ids_match(&t.asset_id, up_token_id) {
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

    let position_up = merge_chain_balance(up, balance_up, escrow_sell_up, data_api_up);
    let position_down = merge_chain_balance(down, balance_down, escrow_sell_down, data_api_down);

    let fills_bootstrap: Vec<Fill> = fills_chrono.into_iter().rev().take(64).collect();

    (position_up, position_down, fills_bootstrap)
}

pub(crate) fn clamp_prob(p: f64) -> f64 {
    p.clamp(0.01, 0.99)
}

/// Derive the OrderArgs + order_type we'd submit for a given user intent,
/// given the current book.
pub fn resolve_market_order(
    state: &AppState,
    outcome: Outcome,
    side: Side,
    size: f64,
    buy_slippage_bps: u32,
    sell_slippage_bps: u32,
) -> Option<(f64, f64, OrderType)> {
    match side {
        Side::Buy => {
            let slip = buy_slippage_bps as f64 / 10_000.0;
            let ask = state.best_ask(outcome)?;
            // Widen ceiling so a thin or stale book still crosses (`MARKET_BUY_SLIPPAGE_BPS`).
            let price = clamp_prob(ask * (1.0 + slip));
            // `size` = USDC notional → shares at reference ask
            let shares = (size / ask).max(0.01);
            Some((shares, price, OrderType::Fak))
        }
        Side::Sell => {
            let slip = sell_slippage_bps as f64 / 10_000.0;
            let bid = state.best_bid(outcome)?;
            let price = clamp_prob(bid * (1.0 - slip));
            // Dump **entire** tracked position (fills + CLOB sync). USDC field only sizes the
            // order when we have no inventory in-app (e.g. before balance sync).
            let held = state.position(outcome).shares.max(0.0);
            let want = (size / bid).max(0.01);
            let shares = if held > 1e-9 { held } else { want };
            if !shares.is_finite() || shares <= 0.0 {
                return None;
            }
            Some((shares, price, OrderType::Fak))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::clob_ws::{BookLevel, BookSnapshot};
    use crate::fees::polymarket_crypto_taker_fee_usdc;
    use crate::trading::{ClobOpenOrder, ClobTrade};

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
            taker_order_id: None,
            maker_orders: vec![],
            trader_side: None,
        }
    }

    #[test]
    fn open_orders_from_clob_filters_and_sorts() {
        let up = "111";
        let down = "222";
        let rows = vec![
            ClobOpenOrder {
                id: String::new(),
                asset_id: down.to_string(),
                side: "SELL".into(),
                price: "0.55".into(),
                original_size: "4".into(),
                size_matched: "1".into(),
            },
            ClobOpenOrder {
                id: String::new(),
                asset_id: up.to_string(),
                side: "BUY".into(),
                price: "0.40".into(),
                original_size: "10".into(),
                size_matched: "0".into(),
            },
        ];
        let out = open_orders_from_clob(rows, up, down);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].outcome, Outcome::Up);
        assert_eq!(out[1].outcome, Outcome::Down);
        assert!((out[1].remaining - 3.0).abs() < 1e-9);
    }

    #[test]
    fn hydrate_vwap_two_buys() {
        let up = "111";
        let down = "222";
        let trades = vec![
            trade("a", up, "BUY", "10", "0.5", "1000"),
            trade("b", up, "BUY", "10", "0.6", "2000"),
        ];
        let (pu, pd, fills) =
            hydrate_positions_from_trades(&trades, up, down, 20.0, 0.0, 0.0, 0.0, None, None);
        assert!((pu.shares - 20.0).abs() < 1e-6);
        // VWAP of USDC cost/share incl. crypto taker fees on each BUY.
        let c1 = 10.0f64.mul_add(0.5, polymarket_crypto_taker_fee_usdc(10.0, 0.5));
        let c2 = 10.0f64.mul_add(0.6, polymarket_crypto_taker_fee_usdc(10.0, 0.6));
        let expect_avg = (c1 + c2) / 20.0;
        assert!((pu.avg_entry - expect_avg).abs() < 1e-6);
        assert!(pd.shares.abs() < 1e-9);
        assert_eq!(fills.len(), 2);
    }

    #[test]
    fn market_sell_sells_full_tracked_position_ignores_small_usdc_ticket() {
        let mut state = AppState::new(5.0);
        state.book_up = Some(BookSnapshot {
            asset_id: "1".into(),
            bids: vec![BookLevel { price: 0.5, size: 1000.0 }],
            asks: vec![BookLevel { price: 0.51, size: 1000.0 }],
        });
        state.position_up.shares = 7.25;
        // $1 ticket → would be 2 shares via USDC/bid; we still exit the full 7.25 sh.
        let (shares, price, _) =
            resolve_market_order(&state, Outcome::Up, Side::Sell, 1.0, 0, 0).unwrap();
        assert!((shares - 7.25).abs() < 1e-9);
        assert!((price - 0.5).abs() < 1e-9);
    }

    #[test]
    fn market_sell_without_position_uses_usdc_over_bid() {
        let mut state = AppState::new(5.0);
        state.book_up = Some(BookSnapshot {
            asset_id: "1".into(),
            bids: vec![BookLevel { price: 0.5, size: 1000.0 }],
            asks: vec![BookLevel { price: 0.51, size: 1000.0 }],
        });
        state.position_up = Position::default();
        let (shares, _, _) =
            resolve_market_order(&state, Outcome::Up, Side::Sell, 10.0, 0, 0).unwrap();
        assert!((shares - 20.0).abs() < 1e-9);
    }

    #[test]
    fn hydrate_sell_realized_in_fill() {
        let up = "111";
        let trades = vec![
            trade("a", up, "BUY", "10", "0.5", "1000"),
            trade("b", up, "SELL", "4", "0.7", "2000"),
        ];
        let (pu, _, fills) =
            hydrate_positions_from_trades(&trades, up, "222", 6.0, 0.0, 0.0, 0.0, None, None);
        assert!((pu.shares - 6.0).abs() < 1e-6);
        let buy_cost = 10.0f64.mul_add(0.5, polymarket_crypto_taker_fee_usdc(10.0, 0.5));
        let expect_avg = buy_cost / 10.0;
        assert!((pu.avg_entry - expect_avg).abs() < 1e-6);
        let sell_fill = fills.iter().find(|f| f.side == Side::Sell).unwrap();
        let expect_realized = 4.0f64.mul_add(0.7, -polymarket_crypto_taker_fee_usdc(4.0, 0.7))
            - 4.0 * expect_avg;
        assert!((sell_fill.realized - expect_realized).abs() < 1e-6);
    }

    /// Spendable conditional balance is low while shares are escrowed for a resting SELL; UI uses trades.
    #[test]
    fn hydrate_prefers_replay_when_balance_excludes_escrow() {
        let up = "111";
        let trades = vec![trade("a", up, "BUY", "10", "0.5", "1000")];
        let spendable = 4.0;
        let (pu, _, _) =
            hydrate_positions_from_trades(&trades, up, "222", spendable, 0.0, 0.0, 0.0, None, None);
        assert!((pu.shares - 10.0).abs() < 1e-6);
        let buy_cost = 10.0f64.mul_add(0.5, polymarket_crypto_taker_fee_usdc(10.0, 0.5));
        assert!((pu.avg_entry - buy_cost / 10.0).abs() < 1e-6);
    }

    #[test]
    fn hydrate_falls_back_to_balance_when_no_trades() {
        let up = "111";
        let trades: Vec<ClobTrade> = vec![];
        let (pu, pd, _) =
            hydrate_positions_from_trades(&trades, up, "222", 3.5, 0.0, 0.0, 0.0, None, None);
        assert!((pu.shares - 3.5).abs() < 1e-6);
        assert!(pd.shares.abs() < 1e-9);
    }

    /// No trade history to replay and spendable balance is 0 while entire position is in a resting SELL.
    #[test]
    fn hydrate_no_trades_uses_sell_escrow_when_spendable_zero() {
        let up = "111";
        let trades: Vec<ClobTrade> = vec![];
        let rows = vec![ClobOpenOrder {
            id: String::new(),
            asset_id: up.to_string(),
            side: "SELL".into(),
            price: "0.60".into(),
            original_size: "12".into(),
            size_matched: "0".into(),
        }];
        let (escrow_u, escrow_d) = escrow_sell_shares_from_clob_orders(&rows, up, "222");
        assert!((escrow_u - 12.0).abs() < 1e-9);
        assert!(escrow_d.abs() < 1e-9);
        let (pu, pd, _) =
            hydrate_positions_from_trades(&trades, up, "222", 0.0, 0.0, escrow_u, escrow_d, None, None);
        assert!((pu.shares - 12.0).abs() < 1e-6);
        assert!(pd.shares.abs() < 1e-9);
    }

    /// CLOB may return `asset_id` as `0x…` while Gamma uses the same id in decimal — replay must still match.
    #[test]
    fn hydrate_replay_matches_trade_when_asset_id_hex_equals_decimal_token() {
        let up_decimal = "10";
        let up_hex = "0x0a";
        let trades = vec![trade("a", up_hex, "BUY", "5", "0.5", "1000")];
        let (pu, _, _) = hydrate_positions_from_trades(
            &trades,
            up_decimal,
            "222",
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
        );
        assert!((pu.shares - 5.0).abs() < 1e-6);
    }

    #[test]
    fn hydrate_uses_data_api_when_trades_and_balances_empty() {
        let trades: Vec<ClobTrade> = vec![];
        let (pu, pd, _) = hydrate_positions_from_trades(
            &trades,
            "111",
            "222",
            0.0,
            0.0,
            0.0,
            0.0,
            Some((8.0, 0.44)),
            None,
        );
        assert!((pu.shares - 8.0).abs() < 1e-6);
        assert!((pu.avg_entry - 0.44).abs() < 1e-6);
        assert!(pd.shares.abs() < 1e-9);
    }
}
