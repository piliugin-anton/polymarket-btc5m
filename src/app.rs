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
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::feeds::{chainlink::PriceTick, clob_ws::BookSnapshot};
use crate::fees::polymarket_crypto_taker_fee_usdc;
use crate::gamma::ActiveMarket;
use crate::gamma_series::SeriesRow;
use crate::market_profile::MarketProfile;
use crate::trailing_stop::{Activation, Side as TrailSide, TickOutcome, TrailSpec, TrailingStop};
use crate::trading::{clob_asset_ids_match, ClobOpenOrder, ClobTrade, OrderType, Side};
use tracing::debug;

/// Minimum outcome shares for a limit (GTD) order — enforced in the UI before submit.
pub const MIN_LIMIT_ORDER_SHARES: f64 = 5.0;

/// How long an [`AppState::order_error_toast`] stays visible.
pub const ORDER_ERROR_TOAST_TTL: Duration = Duration::from_secs(10);

/// Non-modal order error notification (see [`ORDER_ERROR_TOAST_TTL`]).
#[derive(Debug, Clone)]
pub struct OrderErrorToast {
    pub message: String,
    pub until:   Instant,
}

// ── Public event enum ───────────────────────────────────────────────

#[derive(Debug)]
pub enum AppEvent {
    Tick,                         // 1-Hz clock
    Price(PriceTick),
    Book(BookSnapshot),
    MarketRoll(ActiveMarket),
    /// Same slug as the active market — Polymarket crypto-price / Gamma refined `price_to_beat` (e.g. after a window roll). Must not trigger a full roll (positions would reset).
    PriceToBeatRefresh { slug: String, price_to_beat: Option<f64> },
    /// CLOB positions for the current market: balances + cost basis replayed from `GET /data/trades`.
    PositionsLoaded {
        position_up:   Position,
        position_down: Position,
        /// Capped before send — merged into `fills` and sorted by match time (newest first).
        fills_bootstrap: Vec<Fill>,
        /// When false (background poll), keep the current status line (order hints, errors).
        refresh_status_line: bool,
    },
    /// CLOB user-channel `event_type: trade` — P&L / fills (paired with `OrderAck` de-dupe).
    UserChannelFill {
        clob_trade_id:  String,
        order_leg_id:   String,
        side:             Side,
        outcome:        Outcome,
        /// CLOB `asset_id` for this leg (outcome token).
        token_id:       String,
        qty:              f64,
        price:            f64,
        ts:               DateTime<Utc>,
    },
    /// Resting orders for the active market (`GET /data/orders`).
    OpenOrdersLoaded { orders: Vec<OpenOrderRow> },
    /// USDC.e cash + claimable from on-chain reads (Multicall3); neg-risk redeemable sums still use Data API.
    BalancePanelLoaded { cash_usdc: f64, claimable_usdc: f64 },
    /// `GET /holders` — per-`proxyWallet` sums: if a wallet holds both outcomes, only the larger leg counts; then summed per side (Sentiment).
    TopHoldersSentiment { up_sum: f64, down_sum: f64 },
    Key(crossterm::event::KeyEvent),
    /// `clob_order_id` is Polymarket `orderID` from the POST /order body — for fill de-dupe vs user WS.
    /// `token_id` is the CLOB outcome token this fill applies to (must match the order's asset).
    OrderAck {
        side:            Side,
        outcome:       Outcome,
        qty:             f64,
        price:           f64,
        clob_order_id:   Option<String>,
        token_id:        String,
    },
    /// Non-blocking status line only (no modal).
    OrderErr(String),
    /// Order placement / cancel failures: bottom-right toast + status line (auto-dismiss).
    OrderErrModal(String),
    /// Status line update without the `✗` prefix (e.g. claim hint).
    StatusInfo(String),
    /// `POST https://bridge.polymarket.com/deposit` → Solana (`svm`) address + terminal QR art.
    /// `min_deposit_usd` comes from the preceding `/supported-assets` row for Solana USDC.
    SolanaDepositFetched {
        svm_address: String,
        qr_unicode: String,
        min_deposit_usd: Option<f64>,
    },
    SolanaDepositFailed(String),
    /// Wizard: result of `GET /series?slug=…` (per-asset).
    SeriesListReady(std::result::Result<Vec<SeriesRow>, String>),
    /// Wizard complete — start RTDS + Gamma discovery (main spawns tasks; `apply` updates state).
    StartTrading(std::sync::Arc<MarketProfile>),
    /// After a market **Buy** (FAK) when `MARKET_BUY_TRAIL_BPS` is set: register until CLOB **mid**
    /// is at or above **gross** take-profit move from position entry (`MARKET_BUY_TAKE_PROFIT_BPS`).
    RequestTrailingArm {
        outcome:         Outcome,
        /// Fill / REST estimate; used if position not yet updated.
        entry_price:     f64,
        plan_sell_shares: f64,
        token_id:        String,
        trail_bps:       u32,
        /// Arm when `mid / entry >= 1 + activation_bps/10_000` (entry = live `avg_entry` with
        /// open size, else `entry_price`). Same bps as GTD take-profit target, not fee-solved.
        activation_bps:  u32,
        /// Market where this token lives (for cross-market trailing after UI switches).
        market:          ActiveMarket,
    },
    /// Trailing FAK-SELL task finished: `success` when an order was accepted and UI ack sent;
    /// on failure (after retries) clears the stuck trail + pending.
    TrailingExitDispatchDone {
        token_id: String,
        success:  bool,
        error:    Option<String>,
    },
}

// ── UI-level types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome { Up, Down }
impl Outcome {
    pub fn as_str(self) -> &'static str { match self { Outcome::Up => "UP", Outcome::Down => "DOWN" } }
    pub fn opposite(self) -> Self { match self { Outcome::Up => Outcome::Down, Outcome::Down => Outcome::Up } }
}

/// Client-side trailing for one outcome token — fed by CLOB mid (bid/ask average); see [`crate::trailing_stop`].
#[derive(Debug)]
pub struct TrailingSession {
    pub token_id: String,
    /// Market metadata for this token (CLOB `place_order` / tick size).
    pub market: ActiveMarket,
    /// UP/DOWN relative to [`Self::market`] (for status text and user-channel routing).
    pub outcome: Outcome,
    pub stop: TrailingStop,
    /// Shares to target on the follow-up market SELL (capped by live position on trigger).
    pub plan_sell_shares: f64,
    /// Same as at install — used to re-arm after a failed trailing FAK without user action.
    pub trail_bps: u32,
    /// Last known long shares for this token (fills when UI market may point elsewhere).
    pub tracked_shares: f64,
}

/// Queued when the trail trips; `main` submits a FAK SELL (retries if the book is empty).
#[derive(Debug, Clone)]
pub struct TrailingExit {
    pub token_id:    String,
    pub market:      ActiveMarket,
    pub outcome:     Outcome,
    pub sell_shares: f64,
}

/// Trailing is registered after the buy, but the [`TrailingSession`] is created only when
/// **mid** implies a **gross** move of at least `activation_bps` from the position entry.
#[derive(Debug, Clone)]
pub struct PendingTrailArm {
    pub market:          ActiveMarket,
    pub outcome:         Outcome,
    pub entry_price:     f64,
    pub plan_sell_shares: f64,
    pub token_id:        String,
    pub trail_bps:       u32,
    pub activation_bps:  u32,
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
    pub ts:            DateTime<Utc>,
    pub side:          Side,
    pub outcome:       Outcome,
    pub qty:           f64,
    pub price:         f64,
    pub realized:      f64, // only non-zero when the fill closes part of a position
    /// CLOB `Trade.id` from REST or user WebSocket (for de-dupe only; not shown in the TUI).
    pub clob_trade_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    EditSize,
    LimitModal { outcome: Outcome, side: Side, field: LimitField },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitField { Price, Size }

/// Center-screen Polymarket Bridge Solana USDC deposit dialog (`f` key).
#[derive(Debug, Clone)]
pub enum DepositModalPhase {
    Loading,
    Ready {
        svm_address: String,
        qr_unicode: String,
        min_deposit_usd: Option<f64>,
    },
    Failed(String),
}

/// TUI bootstrap: pick asset → timeframe, then the legacy trading layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiPhase {
    WizardLoading,
    WizardPickAsset,
    WizardPickTimeframe,
    Trading,
}

/// Mid price from a CLOB book snapshot (best bid / best ask average).
#[inline]
pub fn book_mid(b: &BookSnapshot) -> Option<f64> {
    let bid = b.bids.first().map(|l| l.price);
    let ask = b.asks.first().map(|l| l.price);
    match (bid, ask) {
        (Some(b), Some(a)) => Some((b + a) / 2.0),
        (Some(b), None) => Some(b),
        (None, Some(a)) => Some(a),
        _ => None,
    }
}

/// Token IDs to subscribe on the public CLOB book WebSocket (active pair + trailing tails).
pub fn collect_book_watch_token_ids(state: &AppState) -> Vec<String> {
    use std::collections::HashSet;
    let mut s: HashSet<String> = HashSet::new();
    if let Some(m) = &state.market {
        s.insert(m.up_token_id.clone());
        s.insert(m.down_token_id.clone());
    }
    for k in state.trailing.keys() {
        s.insert(k.clone());
    }
    for k in state.pending_trail_arms.keys() {
        s.insert(k.clone());
    }
    if let Some(p) = &state.pending_trailing_sell {
        s.insert(p.token_id.clone());
    }
    let mut v: Vec<_> = s.into_iter().collect();
    v.sort();
    v
}

#[derive(Debug)]
pub struct AppState {
    pub ui_phase: UiPhase,
    /// Gamma rows + fallback labels for the first wizard screen.
    pub wizard_rows:        Vec<SeriesRow>,
    pub wizard_list_idx:     usize,
    /// Selected line in timeframe list (0 = 5m, 1 = 15m).
    pub wizard_tf_idx:      usize,
    /// Last load error (wizard still usable via fallback list).
    pub wizard_series_error: Option<String>,

    pub market:         Option<ActiveMarket>,
    /// Chainlink spot (same stream as market resolution for crypto up/down).
    pub spot_price:     Option<f64>,
    pub spot_price_ts:   Option<DateTime<Utc>>,
    /// `arc` of the same profile passed at [`AppEvent::StartTrading`].
    pub market_profile: Option<Arc<MarketProfile>>,

    // One book per outcome token
    pub book_up:        Option<BookSnapshot>,
    pub book_down:      Option<BookSnapshot>,

    pub position_up:    Position,
    pub position_down:  Position,
    pub realized_pnl:   f64,

    /// Net long shares on UP token from FAK/ack path + resync on `PositionsLoaded` (for market SELL sizing).
    pub fak_net_up:     f64,
    /// Net long shares on DOWN token (same).
    pub fak_net_down:   f64,

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

    /// When set, a bottom-right toast is drawn; keys are not blocked. Expires after [`ORDER_ERROR_TOAST_TTL`].
    pub order_error_toast: Option<OrderErrorToast>,

    /// `f` — Solana USDC deposit address + QR (Bridge API).
    pub deposit_modal: Option<DepositModalPhase>,

    /// Net UP vs DOWN after per-wallet “max of UP/DOWN position” (see `fetch_top_holders_amount_sums`); header Sentiment.
    pub top_holders_up_sum:   Option<f64>,
    pub top_holders_down_sum: Option<f64>,

    /// Deduplication between `OrderAck` and user-channel `trade` (see `feeds::user_trade_sync`).
    pub user_trade_sync: Arc<crate::feeds::user_trade_sync::UserTradeSync>,

    /// Trailing stops keyed by CLOB outcome `token_id` (supports multiple markets at once).
    pub trailing: HashMap<String, TrailingSession>,
    /// Buy registered; waiting for **mid** to reach entry × (1 + TP bps) before arming the trail.
    pub pending_trail_arms: HashMap<String, PendingTrailArm>,
    /// Books for tokens that are not the active UI pair but still have trailing / pending arms.
    pub watched_books: HashMap<String, BookSnapshot>,
    /// FAK SELL not yet successfully submitted (empty book, etc.).
    pub pending_trailing_sell: Option<TrailingExit>,
    /// A trailing-exit FAK order is in flight (retries do not spawn duplicate tasks).
    pub trailing_sell_in_flight: bool,
}

impl AppState {
    pub fn new(default_size_usdc: f64, user_trade_sync: Arc<crate::feeds::user_trade_sync::UserTradeSync>) -> Self {
        Self {
            ui_phase:            UiPhase::WizardLoading,
            wizard_rows:         Vec::new(),
            wizard_list_idx:     0,
            wizard_tf_idx:       0,
            wizard_series_error:  None,
            market: None,
            spot_price: None,
            spot_price_ts: None,
            market_profile:       None,
            book_up: None, book_down: None,
            position_up: Default::default(), position_down: Default::default(),
            realized_pnl: 0.0,
            fak_net_up: 0.0,
            fak_net_down: 0.0,
            fills: VecDeque::with_capacity(64),
            open_orders: Vec::new(),
            status_line: "Loading Polymarket markets…".into(),
            collateral_cash_usdc: None,
            collateral_claimable_usdc: None,
            latched_price_to_beat: None,
            default_size_usdc,
            size_input: format!("{default_size_usdc:.2}"),
            limit_price_input: String::new(),
            limit_size_input:  String::new(),
            input_mode: InputMode::Normal,
            order_error_toast: None,
            deposit_modal: None,
            top_holders_up_sum: None,
            top_holders_down_sum: None,
            user_trade_sync,
            trailing: HashMap::new(),
            pending_trail_arms: HashMap::new(),
            watched_books: HashMap::new(),
            pending_trailing_sell: None,
            trailing_sell_in_flight: false,
        }
    }

    // ── Queries ─────────────────────────────────────────────────────
    pub fn price_to_beat(&self) -> Option<f64> {
        self.market
            .as_ref()
            .and_then(|m| m.price_to_beat)
            .or(self.latched_price_to_beat)
    }

    /// Green when spot is at/above the opening "price to beat" (rough UP read).
    pub fn spot_above_target(&self) -> Option<bool> {
        match (self.spot_price, self.price_to_beat()) {
            (Some(p), Some(t)) => Some(p >= t),
            _ => None,
        }
    }

    /// Short label for the header (e.g. "BTC/USD" / "ETH/USD").
    pub fn spot_pair_label(&self) -> String {
        self.market_profile
            .as_ref()
            .map(|p| format!("{}/USD", p.asset.label))
            .unwrap_or_else(|| "—/USD".to_string())
    }

    /// Fraction digits for Chainlink spot / open / delta in the header (XRP needs finer quotes).
    pub fn spot_usd_decimal_places(&self) -> usize {
        match self.market_profile.as_ref().map(|p| p.asset.label) {
            Some("XRP") => 4,
            _ => 2,
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
        book_mid(b)
    }

    /// Mid for a CLOB `token_id`: active UI books first, else [`Self::watched_books`].
    pub fn mid_for_token(&self, token_id: &str) -> Option<f64> {
        if let Some(m) = &self.market {
            if clob_asset_ids_match(token_id, &m.up_token_id) {
                return self.book_up.as_ref().and_then(book_mid);
            }
            if clob_asset_ids_match(token_id, &m.down_token_id) {
                return self.book_down.as_ref().and_then(book_mid);
            }
        }
        self.watched_books.get(token_id).and_then(book_mid)
    }

    /// Best bid for `token_id` (UI or watched book).
    pub fn best_bid_for_token(&self, token_id: &str) -> Option<f64> {
        let b = self.book_snapshot_for_token(token_id)?;
        b.bids.first().map(|l| l.price)
    }

    fn book_snapshot_for_token(&self, token_id: &str) -> Option<&BookSnapshot> {
        if let Some(m) = &self.market {
            if clob_asset_ids_match(token_id, &m.up_token_id) {
                return self.book_up.as_ref();
            }
            if clob_asset_ids_match(token_id, &m.down_token_id) {
                return self.book_down.as_ref();
            }
        }
        self.watched_books.get(token_id)
    }

    /// If `token_id` belongs to the active UI market, return its UP/DOWN side.
    pub fn outcome_for_active_token(&self, token_id: &str) -> Option<Outcome> {
        let m = self.market.as_ref()?;
        if clob_asset_ids_match(token_id, &m.up_token_id) {
            Some(Outcome::Up)
        } else if clob_asset_ids_match(token_id, &m.down_token_id) {
            Some(Outcome::Down)
        } else {
            None
        }
    }

    /// Number of trailing / pending-trail sessions not on the current `market` (if any).
    pub fn background_trail_count(&self) -> usize {
        let Some(m) = self.market.as_ref() else {
            return self.trailing.len() + self.pending_trail_arms.len();
        };
        let mut n = 0;
        for (tid, _) in &self.trailing {
            if !clob_asset_ids_match(tid, &m.up_token_id)
                && !clob_asset_ids_match(tid, &m.down_token_id)
            {
                n += 1;
            }
        }
        for (tid, _) in &self.pending_trail_arms {
            if !clob_asset_ids_match(tid, &m.up_token_id)
                && !clob_asset_ids_match(tid, &m.down_token_id)
            {
                n += 1;
            }
        }
        n
    }

    pub fn position(&self, outcome: Outcome) -> &Position {
        match outcome { Outcome::Up => &self.position_up, Outcome::Down => &self.position_down }
    }
    fn position_mut(&mut self, outcome: Outcome) -> &mut Position {
        match outcome { Outcome::Up => &mut self.position_up, Outcome::Down => &mut self.position_down }
    }

    fn fak_net_mut(&mut self, outcome: Outcome) -> &mut f64 {
        match outcome {
            Outcome::Up => &mut self.fak_net_up,
            Outcome::Down => &mut self.fak_net_down,
        }
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

    /// Drop a trailing plan when the user (or the exchange) reduces position via a SELL fill.
    fn clear_trailing_on_sell_token(&mut self, token_id: &str) {
        self.trailing.remove(token_id);
        self.pending_trail_arms.remove(token_id);
        self.watched_books.remove(token_id);
        if self
            .pending_trailing_sell
            .as_ref()
            .is_some_and(|p| clob_asset_ids_match(&p.token_id, token_id))
        {
            self.pending_trailing_sell = None;
            self.trailing_sell_in_flight = false;
        }
    }

    fn install_trailing_session(
        &mut self,
        market:          ActiveMarket,
        outcome:         Outcome,
        entry_price:     f64,
        plan_sell_shares: f64,
        token_id:        String,
        trail_bps:       u32,
        tracked_shares:  f64,
    ) {
        if trail_bps == 0 || !plan_sell_shares.is_finite() || plan_sell_shares <= 0.0 {
            return;
        }
        let p = (trail_bps as f64 / 10_000.0).max(1e-12);
        let stop = TrailingStop::new(
            TrailSide::Long,
            entry_price,
            TrailSpec::Percent(p),
            Activation::Immediate,
        );
        let tid = token_id.clone();
        self.trailing.insert(
            tid,
            TrailingSession {
                token_id,
                market,
                outcome,
                stop,
                plan_sell_shares,
                trail_bps,
                tracked_shares,
            },
        );
        self.status_line = format!(
            "trailing {out} — {trail_bps} bps trail on mid, SELL up to {plan_sell_shares:.2} sh",
            out = outcome.as_str()
        );
    }

    /// If a pending trail for `token_id` is satisfied by the book and position, move into
    /// [`Self::trailing`]. Entry for the threshold is live `avg_entry` when the outcome has
    /// shares on the **active** UI market for that token, else the pending REST/fill `entry_price`.
    fn try_promote_pending_trail_token(&mut self, token_id: &str) {
        let p0 = match self.pending_trail_arms.get(token_id) {
            Some(n) => n.clone(),
            None => return,
        };
        let bps = p0.activation_bps as f64 / 10_000.0;
        let Some(mid) = self.mid_for_token(token_id) else {
            return;
        };
        let entry = if let Some(oc) = self.outcome_for_active_token(token_id) {
            let pos = self.position(oc);
            if pos.shares > 1e-9 && pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                pos.avg_entry
            } else {
                p0.entry_price
            }
        } else {
            p0.entry_price
        };
        if !entry.is_finite() || entry <= 0.0 {
            return;
        }
        let min_mid = entry * (1.0 + bps);
        if mid + 1e-12 < min_mid {
            return;
        }
        self.pending_trail_arms.remove(token_id);
        let tracked = p0.plan_sell_shares;
        self.install_trailing_session(
            p0.market,
            p0.outcome,
            entry,
            p0.plan_sell_shares,
            p0.token_id,
            p0.trail_bps,
            tracked,
        );
    }

    fn try_promote_pending_trail_any(&mut self, token_id: &str) {
        self.try_promote_pending_trail_token(token_id);
        if let Some(m) = self.market.clone() {
            self.try_promote_pending_trail_token(m.up_token_id.as_str());
            self.try_promote_pending_trail_token(m.down_token_id.as_str());
        }
    }

    fn apply_trailing_book_tick(&mut self, asset_id: &str, mid: f64) {
        let tick = {
            let Some(sess) = self.trailing.get(asset_id) else {
                return;
            };
            if !clob_asset_ids_match(asset_id, &sess.token_id) {
                return;
            }
            sess.stop.on_price(mid)
        };
        let TickOutcome::Triggered { .. } = tick else {
            return;
        };
        let (token_id, market, outcome, plan, tracked, entry_px) = {
            let sess = self.trailing.get(asset_id).expect("trailing session");
            let entry_px = if let Some(oc) = self.outcome_for_active_token(asset_id) {
                let pos = self.position(oc);
                if pos.shares > 1e-9 && pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                    pos.avg_entry
                } else {
                    sess.stop.entry_price()
                }
            } else {
                sess.stop.entry_price()
            };
            (
                sess.token_id.clone(),
                sess.market.clone(),
                sess.outcome,
                sess.plan_sell_shares,
                sess.tracked_shares,
                entry_px,
            )
        };
        if mid > entry_px {
            let pos_sh = if let Some(oc) = self.outcome_for_active_token(asset_id) {
                self.position(oc).shares.max(0.0)
            } else {
                tracked.max(0.0)
            };
            let sh = pos_sh.min(plan).min(tracked.max(0.0));
            if sh > 1e-9 {
                self.pending_trailing_sell = Some(TrailingExit {
                    token_id,
                    market,
                    outcome,
                    sell_shares: sh,
                });
                self.status_line = format!(
                    "trailing: {} tripped (mid {mid:.3} > entry {entry_px:.3}) — SELL {sh:.2} sh",
                    outcome.as_str()
                );
            }
        } else {
            self.status_line = format!(
                "trailing: {} tripped (mid {mid:.3} ≤ entry {entry_px:.3}) — no auto SELL",
                outcome.as_str()
            );
        }
    }

    /// Update [`TrailingSession::tracked_shares`] when fills arrive on a token that may not map
    /// to the active UI pair.
    fn bump_trailing_tracked_shares(&mut self, token_id: &str, side: Side, qty: f64) {
        if let Some(sess) = self.trailing.get_mut(token_id) {
            match side {
                Side::Buy => sess.tracked_shares += qty,
                Side::Sell => sess.tracked_shares = (sess.tracked_shares - qty).max(0.0),
            }
        }
    }

    /// Reserved for PnL on non-UI legs; inventory is tracked via [`Self::bump_trailing_tracked_shares`].
    fn apply_background_trailing_fill(&mut self, _token_id: &str, _side: Side, _qty: f64) {}

    // ── Mutations ───────────────────────────────────────────────────
    pub fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Tick => {
                if self
                    .order_error_toast
                    .as_ref()
                    .is_some_and(|t| Instant::now() >= t.until)
                {
                    self.order_error_toast = None;
                }
            }
            AppEvent::Price(p) => {
                self.spot_price    = Some(p.price);
                self.spot_price_ts = chrono::DateTime::<Utc>::from_timestamp_millis(p.timestamp_ms as i64);
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
                let aid = b.asset_id.clone();
                let aid_s = aid.as_str();
                if let Some(m) = &self.market {
                    if clob_asset_ids_match(aid_s, &m.up_token_id) {
                        self.book_up = Some(b.clone());
                    } else if clob_asset_ids_match(aid_s, &m.down_token_id) {
                        self.book_down = Some(b.clone());
                    }
                }
                let watch = collect_book_watch_token_ids(self);
                if watch.iter().any(|t| clob_asset_ids_match(t, aid_s)) {
                    self.watched_books.insert(aid.clone(), b);
                }
                let keep: std::collections::HashSet<String> =
                    watch.into_iter().collect();
                self.watched_books.retain(|k, _| keep.contains(k));

                self.try_promote_pending_trail_any(aid_s);
                if let Some(mid) = self.mid_for_token(aid_s) {
                    self.apply_trailing_book_tick(aid_s, mid);
                }
            }
            AppEvent::MarketRoll(m) => {
                // Close any positions from the previous market — they'll resolve
                // via Polymarket and show up as realized once winnings redeem.
                // We keep realized_pnl but zero out live positions for the new market.
                // Return to normal keys (w/s/a/d/…) — otherwise `e` / limit modal survives a roll.
                // Deposit modal (`f`) stays open across rolls.
                self.input_mode = InputMode::Normal;
                self.order_error_toast = None;
                self.status_line = format!("New market: {}", m.question);
                self.latched_price_to_beat = None;
                self.market = Some(m);
                self.book_up = None; self.book_down = None;
                self.position_up = Default::default();
                self.position_down = Default::default();
                self.fak_net_up = 0.0;
                self.fak_net_down = 0.0;
                self.open_orders.clear();
                self.top_holders_up_sum = None;
                self.top_holders_down_sum = None;
                self.watched_books.clear();
            }
            AppEvent::PriceToBeatRefresh { slug, price_to_beat } => {
                if let Some(m) = &mut self.market {
                    if m.slug == slug {
                        m.price_to_beat = price_to_beat;
                        if price_to_beat.is_some() {
                            self.latched_price_to_beat = None;
                        }
                    }
                }
            }
            AppEvent::PositionsLoaded {
                position_up,
                position_down,
                fills_bootstrap,
                refresh_status_line,
            } => {
                self.position_up = position_up;
                self.position_down = position_down;
                self.fills.clear();
                let mut seen_seed: Vec<String> = Vec::new();
                for f in fills_bootstrap {
                    if let Some(ref tid) = f.clob_trade_id {
                        if !tid.is_empty() {
                            seen_seed.push(tid.clone());
                        }
                    }
                    self.fills.push_back(f);
                }
                self.user_trade_sync.seed_seen_from_hydration(seen_seed);
                trim_fills_to_cap(&mut self.fills, 64);
                let nu = net_shares_from_fills(&self.fills, Outcome::Up).max(0.0);
                let nd = net_shares_from_fills(&self.fills, Outcome::Down).max(0.0);
                self.fak_net_up = self.position_up.shares.max(nu);
                self.fak_net_down = self.position_down.shares.max(nd);
                if refresh_status_line {
                    self.status_line = format!(
                        "Positions from CLOB — UP {:.2} @ {:.3} / DOWN {:.2} @ {:.3}",
                        self.position_up.shares,
                        self.position_up.avg_entry,
                        self.position_down.shares,
                        self.position_down.avg_entry,
                    );
                }
                if let Some(m) = self.market.clone() {
                    self.try_promote_pending_trail_token(m.up_token_id.as_str());
                    self.try_promote_pending_trail_token(m.down_token_id.as_str());
                }
            }
            AppEvent::OpenOrdersLoaded { orders } => {
                self.open_orders = orders;
            }
            AppEvent::BalancePanelLoaded { cash_usdc, claimable_usdc } => {
                self.collateral_cash_usdc = Some(cash_usdc);
                self.collateral_claimable_usdc = Some(claimable_usdc);
            }
            AppEvent::TopHoldersSentiment { up_sum, down_sum } => {
                self.top_holders_up_sum = Some(up_sum);
                self.top_holders_down_sum = Some(down_sum);
            }
            AppEvent::UserChannelFill {
                clob_trade_id,
                order_leg_id,
                side,
                outcome,
                token_id,
                qty,
                price,
                ts,
            } => {
                if let Some(ui_oc) = self.outcome_for_active_token(&token_id) {
                    let realized = self.position_mut(ui_oc).apply_fill(side, qty, price);
                    self.realized_pnl += realized;
                    match side {
                        Side::Buy => *self.fak_net_mut(ui_oc) += qty,
                        Side::Sell => {
                            let v = self.fak_net_mut(ui_oc);
                            *v = (*v - qty).max(0.0);
                        }
                    }
                    self.fills.push_back(Fill {
                        ts,
                        side,
                        outcome: ui_oc,
                        qty,
                        price,
                        realized,
                        clob_trade_id: if clob_trade_id.is_empty() {
                            None
                        } else {
                            Some(clob_trade_id.clone())
                        },
                    });
                    trim_fills_to_cap(&mut self.fills, 64);
                    self.user_trade_sync
                        .after_ws_user_fill_committed(&clob_trade_id, &order_leg_id, qty, price);
                } else {
                    self.apply_background_trailing_fill(&token_id, side, qty);
                    self.user_trade_sync
                        .after_ws_user_fill_committed(&clob_trade_id, &order_leg_id, qty, price);
                }
                self.bump_trailing_tracked_shares(&token_id, side, qty);
                if side == Side::Sell {
                    self.clear_trailing_on_sell_token(&token_id);
                } else {
                    self.try_promote_pending_trail_any(token_id.as_str());
                }
                self.status_line = format!(
                    "{} {qty:.2} {} @ {price:.3} (WSS trade)",
                    side_str(side),
                    outcome.as_str()
                );
            }
            AppEvent::OrderAck {
                side,
                outcome,
                qty,
                price,
                clob_order_id,
                token_id,
            } => {
                let mut do_pnl = true;
                if let Some(ref oid) = clob_order_id {
                    if !self.user_trade_sync.before_order_ack_apply(oid, qty, price) {
                        do_pnl = false;
                    }
                }
                if do_pnl {
                    if let Some(ui_oc) = self.outcome_for_active_token(&token_id) {
                        let realized = self.position_mut(ui_oc).apply_fill(side, qty, price);
                        self.realized_pnl += realized;
                        match side {
                            Side::Buy => *self.fak_net_mut(ui_oc) += qty,
                            Side::Sell => {
                                let v = self.fak_net_mut(ui_oc);
                                *v = (*v - qty).max(0.0);
                            }
                        }
                        self.fills.push_back(Fill {
                            ts:            Utc::now(),
                            side,
                            outcome: ui_oc,
                            qty,
                            price,
                            realized,
                            clob_trade_id: None,
                        });
                    } else {
                        self.apply_background_trailing_fill(&token_id, side, qty);
                    }
                    trim_fills_to_cap(&mut self.fills, 64);
                    if let Some(oid) = clob_order_id {
                        self.user_trade_sync.after_order_ack_applied(&oid, qty, price);
                    }
                    self.bump_trailing_tracked_shares(&token_id, side, qty);
                    if side == Side::Sell {
                        self.clear_trailing_on_sell_token(&token_id);
                    } else {
                        self.try_promote_pending_trail_any(token_id.as_str());
                    }
                }
                self.status_line = format!(
                    "{} {qty:.2} {} @ {price:.3} ✓",
                    side_str(side),
                    outcome.as_str()
                );
            }
            AppEvent::RequestTrailingArm {
                outcome,
                entry_price,
                plan_sell_shares,
                token_id,
                trail_bps,
                activation_bps,
                market,
            } => {
                if trail_bps == 0 || !plan_sell_shares.is_finite() || plan_sell_shares <= 0.0 {
                    // Misconfiguration — main should not send; ignore.
                } else {
                    let tid = token_id.clone();
                    self.pending_trail_arms.insert(
                        tid.clone(),
                        PendingTrailArm {
                            market,
                            outcome,
                            entry_price,
                            plan_sell_shares,
                            token_id,
                            trail_bps,
                            activation_bps,
                        },
                    );
                    self.status_line = format!(
                        "trailing: {} pending — arm when mid ≥ entry×(1+{activation_bps} bps) (from position)",
                        outcome.as_str()
                    );
                    self.try_promote_pending_trail_any(tid.as_str());
                }
            }
            AppEvent::OrderErr(e) => self.status_line = format!("✗ {e}"),
            AppEvent::OrderErrModal(e) => {
                self.status_line = format!("✗ {e}");
                self.order_error_toast = Some(OrderErrorToast {
                    message: e,
                    until:   Instant::now() + ORDER_ERROR_TOAST_TTL,
                });
            }
            AppEvent::StatusInfo(msg) => self.status_line = msg,
            AppEvent::SolanaDepositFetched {
                svm_address,
                qr_unicode,
                min_deposit_usd,
            } => {
                if matches!(self.deposit_modal, Some(DepositModalPhase::Loading)) {
                    self.deposit_modal = Some(DepositModalPhase::Ready {
                        svm_address,
                        qr_unicode,
                        min_deposit_usd,
                    });
                }
            }
            AppEvent::SolanaDepositFailed(msg) => {
                if matches!(self.deposit_modal, Some(DepositModalPhase::Loading)) {
                    self.deposit_modal = Some(DepositModalPhase::Failed(msg));
                }
            }
            AppEvent::SeriesListReady(res) => {
                match res {
                    Ok(rows) if !rows.is_empty() => {
                        self.wizard_rows = rows;
                        self.wizard_series_error = None;
                    }
                    Ok(_) => {
                        self.wizard_rows = crate::gamma_series::static_fallback_rows();
                        self.wizard_series_error = None;
                    }
                    Err(e) => {
                        self.wizard_series_error = Some(e);
                        self.wizard_rows = crate::gamma_series::static_fallback_rows();
                    }
                }
                if self.wizard_rows.is_empty() {
                    self.wizard_rows = crate::gamma_series::static_fallback_rows();
                }
                self.ui_phase = UiPhase::WizardPickAsset;
                self.wizard_list_idx = 0;
                self.wizard_tf_idx = 0;
                self.status_line = "Select asset (↑/↓ Enter) — Q quit".into();
            }
            AppEvent::StartTrading(p) => {
                // Clear the previous market’s UI until Gamma resolves the new profile’s window.
                self.input_mode = InputMode::Normal;
                self.order_error_toast = None;
                self.spot_price = None;
                self.spot_price_ts = None;
                self.market = None;
                self.book_up = None;
                self.book_down = None;
                self.latched_price_to_beat = None;
                self.position_up = Default::default();
                self.position_down = Default::default();
                self.fak_net_up = 0.0;
                self.fak_net_down = 0.0;
                self.open_orders.clear();
                self.top_holders_up_sum = None;
                self.top_holders_down_sum = None;
                self.watched_books.clear();
                self.market_profile = Some(p.clone());
                self.ui_phase = UiPhase::Trading;
                self.status_line = "Waiting for market data…".into();
            }
            AppEvent::TrailingExitDispatchDone {
                token_id,
                success,
                error,
            } => {
                self.trailing_sell_in_flight = false;
                let tid = token_id.clone();
                if !success {
                    let had_sess = self.trailing.remove(&tid);
                    let session_outcome = had_sess.as_ref().map(|s| s.outcome);
                    let mut re_armed = false;
                    if let Some(sess) = had_sess {
                        let pos_sh = if let Some(oc) = self.outcome_for_active_token(&sess.token_id)
                        {
                            self.position(oc).shares
                        } else {
                            sess.tracked_shares
                        };
                        if pos_sh > 1e-9
                            && sess.trail_bps > 0
                            && sess.plan_sell_shares.is_finite()
                            && sess.plan_sell_shares > 0.0
                        {
                            let entry = if let Some(oc) = self.outcome_for_active_token(&sess.token_id)
                            {
                                let pos = self.position(oc);
                                if pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                                    pos.avg_entry
                                } else {
                                    sess.stop.entry_price()
                                }
                            } else {
                                sess.stop.entry_price()
                            };
                            self.install_trailing_session(
                                sess.market.clone(),
                                sess.outcome,
                                entry,
                                sess.plan_sell_shares,
                                sess.token_id.clone(),
                                sess.trail_bps,
                                pos_sh.max(0.0),
                            );
                            re_armed = true;
                        }
                    }
                    if self
                        .pending_trailing_sell
                        .as_ref()
                        .is_some_and(|p| clob_asset_ids_match(&p.token_id, &tid))
                    {
                        self.pending_trailing_sell = None;
                    }
                    if let Some(e) = error {
                        let oc = session_outcome.unwrap_or(Outcome::Up);
                        self.status_line = if re_armed {
                            format!("✗ {e} — trailing {} re-armed (live entry)", oc.as_str())
                        } else {
                            format!("✗ {e}")
                        };
                        self.order_error_toast = Some(OrderErrorToast {
                            message: e,
                            until:   Instant::now() + ORDER_ERROR_TOAST_TTL,
                        });
                    } else if re_armed {
                        let oc = session_outcome.unwrap_or(Outcome::Up);
                        self.status_line =
                            format!("trailing {} re-armed after exit failure", oc.as_str());
                    }
                } else {
                    self.trailing.remove(&tid);
                    if self
                        .pending_trailing_sell
                        .as_ref()
                        .is_some_and(|p| clob_asset_ids_match(&p.token_id, &tid))
                    {
                        self.pending_trailing_sell = None;
                    }
                }
            }
            AppEvent::Key(_) => {} // handled in main via `events::handle_key`
        }
    }
}

fn side_str(s: Side) -> &'static str { match s { Side::Buy => "BUY", Side::Sell => "SELL" } }

/// `VecDeque` front = latest `Fill::ts` (newest match time first).
fn sort_fills_by_ts_desc(fills: &mut VecDeque<Fill>) {
    let mut v: Vec<_> = fills.drain(..).collect();
    v.sort_unstable_by(|a, b| b.ts.cmp(&a.ts));
    fills.extend(v);
}

fn trim_fills_to_cap(fills: &mut VecDeque<Fill>, cap: usize) {
    sort_fills_by_ts_desc(fills);
    while fills.len() > cap {
        fills.pop_back();
    }
}

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

    // Trade replay can lag on-chain / POST rounding (e.g. buy fill vs USDC-notional estimate).
    // Prefer the larger of replay and spendable+escrow so market SELL can flatten fully.
    let shares = if replay_ok {
        replay
            .shares
            .max(if inventory_fallback > 1e-12 { inventory_fallback } else { 0.0 })
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
            clob_trade_id: Some(t.id.clone()),
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

/// Net long shares for `outcome` from the session fill log (order of entries does not matter).
pub fn net_shares_from_fills(fills: &VecDeque<Fill>, outcome: Outcome) -> f64 {
    let mut net = 0.0f64;
    for f in fills.iter() {
        if f.outcome != outcome {
            continue;
        }
        match f.side {
            Side::Buy => net += f.qty,
            Side::Sell => net -= f.qty,
        }
    }
    net
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
            // Market FAK is an aggressive limit: ceiling above best ask so the book still crosses
            // if the snapshot is stale or the ask moves (`MARKET_BUY_SLIPPAGE_BPS`; `0` = no cushion).
            let price = clamp_prob(ask * (1.0 + slip));
            // `size` = USDC notional → shares at reference ask
            let shares = (size / ask).max(0.01);
            Some((shares, price, OrderType::Fak))
        }
        Side::Sell => {
            let slip = sell_slippage_bps as f64 / 10_000.0;
            let bid = state.best_bid(outcome)?;
            // Floor below best bid (`MARKET_SELL_SLIPPAGE_BPS`; `0` = no cushion).
            let price = clamp_prob(bid * (1.0 - slip));
            // Dump **entire** inventory: position (replay ∪ balance in merge) and fill-implied net,
            // so we do not leave dust when VWAP state rounds below actual fills / wallet.
            let held = state.position(outcome).shares.max(0.0);
            let fill_net = net_shares_from_fills(&state.fills, outcome).max(0.0);
            let fak_net = match outcome {
                Outcome::Up => state.fak_net_up,
                Outcome::Down => state.fak_net_down,
            }
            .max(0.0);
            let want = (size / bid).max(0.01);
            let inventory = held.max(fill_net).max(fak_net);
            let shares = if inventory > 1e-9 { inventory } else { want };
            if !shares.is_finite() || shares <= 0.0 {
                return None;
            }
            Some((shares, price, OrderType::Fak))
        }
    }
}

/// FAK SELL pricing for a trailing exit on an arbitrary `token_id` (UI or watched book).
pub fn resolve_trailing_sell(
    state: &AppState,
    token_id: &str,
    sell_shares: f64,
    sell_slippage_bps: u32,
) -> Option<(f64, f64, OrderType)> {
    let slip = sell_slippage_bps as f64 / 10_000.0;
    let bid = state.best_bid_for_token(token_id)?;
    let price = clamp_prob(bid * (1.0 - slip));
    if !sell_shares.is_finite() || sell_shares <= 0.0 {
        return None;
    }
    Some((sell_shares, price, OrderType::Fak))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use chrono::Utc;
    use crate::feeds::clob_ws::{BookLevel, BookSnapshot};
    use crate::feeds::user_trade_sync::UserTradeSync;
    use crate::fees::polymarket_crypto_taker_fee_usdc;
    use crate::gamma::ActiveMarket;
    use crate::trading::{ClobOpenOrder, ClobTrade};

    fn test_state() -> AppState {
        AppState::new(5.0, Arc::new(UserTradeSync::new()))
    }

    fn test_market(up: &str, down: &str, cond: &str) -> ActiveMarket {
        ActiveMarket {
            condition_id: cond.into(),
            question:     "test".into(),
            slug:         "test".into(),
            up_token_id:  up.into(),
            down_token_id: down.into(),
            tick_size:    "0.01".into(),
            neg_risk:     false,
            price_to_beat: None,
            opens_at:     Utc::now(),
            closes_at:    Utc::now(),
            crypto_price_query_start_utc: String::new(),
            crypto_price_query_end_utc:   String::new(),
        }
    }

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
        let mut state = test_state();
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
        let mut state = test_state();
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
    fn market_sell_uses_fill_net_when_larger_than_position() {
        let mut state = test_state();
        state.book_up = Some(BookSnapshot {
            asset_id: "1".into(),
            bids: vec![BookLevel { price: 0.5, size: 1000.0 }],
            asks: vec![BookLevel { price: 0.51, size: 1000.0 }],
        });
        state.position_up.shares = 10.0;
        state.fills.push_back(Fill {
            ts: Utc::now(),
            side: Side::Buy,
            outcome: Outcome::Up,
            qty: 10.09,
            price: 0.5,
            realized: 0.0,
            clob_trade_id: None,
        });
        let (shares, _, _) =
            resolve_market_order(&state, Outcome::Up, Side::Sell, 1.0, 0, 0).unwrap();
        assert!((shares - 10.09).abs() < 1e-9);
    }

    #[test]
    fn market_sell_uses_fak_net_when_larger_than_position_and_fills() {
        let mut state = test_state();
        state.book_up = Some(BookSnapshot {
            asset_id: "1".into(),
            bids: vec![BookLevel { price: 0.5, size: 1000.0 }],
            asks: vec![BookLevel { price: 0.51, size: 1000.0 }],
        });
        state.position_up.shares = 10.0;
        state.fak_net_up = 10.11;
        let (shares, _, _) =
            resolve_market_order(&state, Outcome::Up, Side::Sell, 1.0, 0, 0).unwrap();
        assert!((shares - 10.11).abs() < 1e-9);
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

    /// On-chain / allowance balance can slightly exceed summed trade sizes (rounding); size for exit uses the max.
    #[test]
    fn hydrate_replay_combined_with_higher_chain_inventory() {
        let up = "111";
        let trades = vec![trade("a", up, "BUY", "10", "0.5", "1000")];
        let chain = 10.09;
        let (pu, _, _) =
            hydrate_positions_from_trades(&trades, up, "222", chain, 0.0, 0.0, 0.0, None, None);
        assert!((pu.shares - 10.09).abs() < 1e-6);
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

    /// Trailing on token `OLD_UP` stays in state when the UI `market` is already a different pair.
    #[test]
    fn trailing_session_survives_different_ui_market() {
        let mut s = test_state();
        let m_old = test_market("OLD_UP", "OLD_DOWN", "0xc0");
        let m_new = test_market("NEW_UP", "NEW_DOWN", "0xc1");
        s.market = Some(m_new.clone());
        s.apply(AppEvent::RequestTrailingArm {
            outcome:          Outcome::Up,
            entry_price:      0.5,
            plan_sell_shares: 10.0,
            token_id:         "OLD_UP".into(),
            trail_bps:        100,
            activation_bps:   0,
            market:           m_old,
        });
        let b_old = BookSnapshot {
            asset_id: "OLD_UP".into(),
            bids:     vec![BookLevel { price: 0.49, size: 10.0 }],
            asks:     vec![BookLevel { price: 0.51, size: 10.0 }],
        };
        s.apply(AppEvent::Book(b_old));
        assert!(
            s.trailing.contains_key("OLD_UP") || s.pending_trail_arms.contains_key("OLD_UP"),
            "expected pending or armed trail on OLD_UP"
        );
        let b_new = BookSnapshot {
            asset_id: "NEW_UP".into(),
            bids:     vec![BookLevel { price: 0.40, size: 5.0 }],
            asks:     vec![BookLevel { price: 0.42, size: 5.0 }],
        };
        s.apply(AppEvent::Book(b_new));
        assert!(
            s.trailing.contains_key("OLD_UP") || s.pending_trail_arms.contains_key("OLD_UP"),
            "OLD_UP trail should not be dropped when NEW_UP book updates"
        );
        assert!(s.mid_for_token("OLD_UP").is_some());
    }
}
