//! Central application state + event dispatch.
//!
//! Three async sources push into a single `AppEvent` channel:
//!   1. crossterm key events
//!   2. Chainlink price ticks
//!   3. CLOB book snapshots + public trade prints (`last_trade_price`)
//!   4. CLOB conditional balances (positions) after each market roll
//!   5. Periodic ticks for market rolling
//!
//! Trading actions post results back through the same channel.

use chrono::{DateTime, Utc};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::feeds::{chainlink::PriceTick, clob_ws::BookSnapshot};
use crate::fees::polymarket_crypto_taker_fee_usdc;
use crate::gamma::ActiveMarket;
use crate::gamma_series::SeriesRow;
use crate::market_activity::RollingTradedNotional;
use crate::market_profile::MarketProfile;
use crate::strategy::{ManualSignalBookSide, ManualSignalInput, ManualSignalLabel, ManualSignalSentiment};
use crate::stop_loss::{stop_loss_sell_limit_price, stop_loss_triggered};
use crate::take_profit::{
    clob_order_has_open_size, clob_order_remaining_size, CLOB_ORDER_REMAINING_DUST,
};
use crate::trading::{
    canonical_clob_token_id, clob_asset_ids_match, norm_clob_owner, norm_order_id_key,
    parse_clob_side_str, taker_trade_fill_shares, ClobMakerOrder, ClobOpenOrder, ClobTrade,
    OrderType, Side,
};
use crate::trailing_stop::{Activation, Side as TrailSide, TickOutcome, TrailSpec, TrailingStop};
use tracing::debug;

/// Minimum outcome shares for a limit (GTD) order — enforced in the UI before submit.
pub const MIN_LIMIT_ORDER_SHARES: f64 = 5.0;

/// Max concurrent in-flight [`AppEvent::TrailingExitDispatchDone`] FAK SELL runs (per-token, bounded
/// like `tokio::sync::Semaphore(N)` to limit API / exchange load; see `try_dispatch_trailing_sell` in
/// `main.rs`).
pub const TRAILING_SELL_MAX_PARALLEL: usize = 8;
/// After a trailing FAK partially fills, user-channel fills for the same CLOB order can arrive a few
/// seconds later. During this window, keep the residual trail from firing a duplicate SELL.
pub const TRAILING_PARTIAL_FILL_GRACE: Duration = Duration::from_secs(3);
const AUTOTRADING_POSITION_DUST: f64 = 1e-9;

/// Map key in [`AppState::trailing`] for this CLOB asset.
/// Two hash lookups: direct, then canonical form. Keys are always canonical, so this covers all cases.
fn trailing_map_key_for_asset(
    trailing: &HashMap<String, TrailingSession>,
    asset_id: &str,
) -> Option<String> {
    if trailing.contains_key(asset_id) {
        return Some(asset_id.to_string());
    }
    let c = canonical_clob_token_id(asset_id);
    if c.as_ref() != asset_id && trailing.contains_key(c.as_ref()) {
        return Some(c.into_owned());
    }
    None
}

/// Map key in [`AppState::pending_trail_arms`] for this CLOB asset.
fn pending_trail_map_key_for_asset(
    pending: &HashMap<String, PendingTrailArm>,
    asset_id: &str,
) -> Option<String> {
    if pending.contains_key(asset_id) {
        return Some(asset_id.to_string());
    }
    let c = canonical_clob_token_id(asset_id);
    if c.as_ref() != asset_id && pending.contains_key(c.as_ref()) {
        return Some(c.into_owned());
    }
    None
}

/// How long an [`AppState::order_error_toast`] stays visible.
pub const ORDER_ERROR_TOAST_TTL: Duration = Duration::from_secs(10);

/// Non-modal order error notification (see [`ORDER_ERROR_TOAST_TTL`]).
#[derive(Debug, Clone)]
pub struct OrderErrorToast {
    pub message: String,
    pub until: Instant,
}

// ── Public event enum ───────────────────────────────────────────────

#[derive(Debug)]
pub enum AppEvent {
    Tick, // 1-Hz clock
    Price(PriceTick),
    Book(BookSnapshot),
    /// Coalesced public CLOB market WS `last_trade_price` prints.
    ClobPublicTrades(Vec<crate::feeds::clob_ws::ClobTradePrint>),
    /// Seed rolling activity from Data API `GET /trades` after a market roll.
    ActivityTradesSeed(Vec<crate::feeds::clob_ws::ClobTradePrint>),
    /// New active market + buy-side trail settings (from env at roll; used for maker WSS fills).
    MarketRoll {
        market: ActiveMarket,
        buy_trail_bps: u32,
        buy_trail_activation_bps: u32,
    },
    /// Same slug as the active market — Polymarket crypto-price / Gamma refined `price_to_beat` (e.g. after a window roll). Must not trigger a full roll (positions would reset).
    PriceToBeatRefresh {
        slug: String,
        price_to_beat: Option<f64>,
    },
    /// CLOB positions for the current market: balances + cost basis replayed from `GET /data/trades`.
    PositionsLoaded {
        position_up: Position,
        position_down: Position,
        /// Capped before send — merged into `fills` and sorted by match time (newest first).
        fills_bootstrap: Vec<Fill>,
        /// When false (background poll), keep the current status line (order hints, errors).
        refresh_status_line: bool,
    },
    /// CLOB user-channel `event_type: trade` — P&L / fills (paired with `OrderAck` de-dupe).
    UserChannelFill {
        clob_trade_id: String,
        order_leg_id: String,
        side: Side,
        outcome: Outcome,
        /// CLOB `asset_id` for this leg (outcome token).
        token_id: String,
        qty: f64,
        price: f64,
        ts: DateTime<Utc>,
        /// `trader_side == MAKER` on the user-channel trade — resting limit fills (vs taker / FAK).
        from_maker_leg: bool,
    },
    /// Resting orders for the active market (`GET /data/orders`).
    OpenOrdersLoaded {
        orders: Vec<OpenOrderRow>,
    },
    /// USDC.e cash + claimable from on-chain reads (Multicall3); neg-risk redeemable sums still use Data API.
    BalancePanelLoaded {
        cash_usdc: f64,
        claimable_usdc: f64,
        claimable_refreshed: bool,
    },
    /// `GET /holders` — per-`proxyWallet` sums: if a wallet holds both outcomes, only the larger leg counts; then summed per side (Sentiment).
    TopHoldersSentiment {
        up_sum: f64,
        down_sum: f64,
    },
    Key(crossterm::event::KeyEvent),
    /// `clob_order_id` is Polymarket `orderID` from the POST /order body — for fill de-dupe vs user WS.
    /// `token_id` is the CLOB outcome token this fill applies to (must match the order's asset).
    OrderAck {
        side: Side,
        outcome: Outcome,
        qty: f64,
        price: f64,
        clob_order_id: Option<String>,
        token_id: String,
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
    /// After a **Buy** (FAK or GTD with fill in POST) with `TAKE_PROFIT_BPS > 0` and `BUY_TRAIL_BPS == 0`:
    /// consolidate take-profit vs open SELL legs and current position (handled in `main::apply_app_event`).
    RunTakeProfitAfterBuy {
        market: crate::gamma::ActiveMarket,
        outcome: Outcome,
        take_profit_bps: u32,
        /// Same `qty` as the preceding [`AppEvent::OrderAck`] for this FAK BUY (CLOB fill estimate).
        buy_ack_qty: f64,
    },
    /// User-channel `order` updates show **≥2** resting SELL on the same outcome — merge into one
    /// GTD (handled in `main::apply_app_event`).
    MergeTakeProfitRestingSells {
        outcome: Outcome,
    },
    /// After a **Buy** (FAK or GTD with fill in POST) when `BUY_TRAIL_BPS` is set: register until CLOB **mid**
    /// is at or above **gross** take-profit move from position entry (`TAKE_PROFIT_BPS`).
    RequestTrailingArm {
        outcome: Outcome,
        /// Fill / REST estimate; used if position not yet updated.
        entry_price: f64,
        plan_sell_shares: f64,
        token_id: String,
        trail_bps: u32,
        /// Arm when `mid / entry >= 1 + activation_bps/10_000` (entry = live `avg_entry` with
        /// open size, else `entry_price`). Same bps as GTD take-profit target, not fee-solved.
        activation_bps: u32,
        /// Market where this token lives (for cross-market trailing after UI switches).
        market: ActiveMarket,
    },
    /// Trailing FAK-SELL task finished: `success` when an order was accepted and UI ack sent;
    /// on failure (after retries) clears the stuck trail + pending.
    TrailingExitDispatchDone {
        token_id: String,
        success: bool,
        error: Option<String>,
    },
    /// Stop-loss GTD SELL task finished; clears the duplicate-submit in-flight marker.
    StopLossSellDone {
        token_id: String,
        success: bool,
        error: Option<String>,
    },
    /// Auto-trading GTD limit BUY task finished. Errors free the reserved capacity immediately;
    /// accepted resting orders keep it reserved until a fill arrives from the user channel or the
    /// order is cancelled/expired.
    AutoTradingBuyDone {
        token_id: String,
        order_id: Option<String>,
        filled_qty: Option<f64>,
        error: Option<String>,
    },
    /// User-channel `order` event says a previously accepted auto BUY was cancelled/expired.
    AutoTradingBuyOrderClosed {
        order_id: String,
    },
    /// Manual or automatic redeem-all task finished; clears the shared redeem in-flight guard.
    RedeemDone {
        auto: bool,
        result: std::result::Result<String, String>,
    },
}

// ── UI-level types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    Up,
    Down,
}
impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Up => "UP",
            Outcome::Down => "DOWN",
        }
    }
    pub fn opposite(self) -> Self {
        match self {
            Outcome::Up => Outcome::Down,
            Outcome::Down => Outcome::Up,
        }
    }
}

/// Client-side trailing for one outcome token — fed by CLOB best bid; see [`crate::trailing_stop`].
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
    pub token_id: String,
    pub market: ActiveMarket,
    pub outcome: Outcome,
    pub sell_shares: f64,
}

/// Stop-loss GTD sell to dispatch from `main` after a book tick crosses the configured drawdown.
#[derive(Debug, Clone)]
pub struct StopLossSell {
    pub token_id: String,
    pub market: ActiveMarket,
    pub outcome: Outcome,
    pub shares: f64,
    pub reference_price: f64,
    pub current_price: f64,
    pub limit_price: f64,
}

/// Trailing is registered after the buy, but the [`TrailingSession`] is created only when
/// **mid** implies a **gross** move of at least `activation_bps` from the position entry.
#[derive(Debug, Clone)]
pub struct PendingTrailArm {
    pub market: ActiveMarket,
    pub outcome: Outcome,
    pub entry_price: f64,
    pub plan_sell_shares: f64,
    pub token_id: String,
    pub trail_bps: u32,
    pub activation_bps: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub shares: f64,
    /// Volume-weighted average **USDC cost per share**, including Polymarket **taker** fees on buys
    /// (Crypto: `fee = C × 0.072 × p × (1-p)` per [fees](https://docs.polymarket.com/trading/fees)).
    pub avg_entry: f64,
}
impl Position {
    fn add(&mut self, qty: f64, price: f64) {
        let new_total = self.shares + qty;
        if new_total.abs() < 1e-9 {
            *self = Default::default();
            return;
        }
        let buy_cost = qty.mul_add(price, polymarket_crypto_taker_fee_usdc(qty, price));
        self.avg_entry = (self.shares * self.avg_entry + buy_cost) / new_total;
        self.shares = new_total;
    }
    fn reduce(&mut self, qty: f64, price: f64) -> f64 {
        // returns realized pnl for the portion closed (taker fee on sell deducted)
        let closed = qty.min(self.shares);
        let fee_out = polymarket_crypto_taker_fee_usdc(closed, price);
        let pnl = closed.mul_add(price, -fee_out) - closed * self.avg_entry;
        self.shares -= closed;
        if self.shares.abs() < 1e-9 {
            *self = Default::default();
        }
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

    /// BUY from exchange-reported **total USDC** and share count (e.g. L2 `makingAmount` / `takingAmount`
    /// on a taker buy). `total_usdc` is the full debit for that lot—**no** extra taker-fee line, unlike
    /// [`Self::add`], so REST-replay cost basis matches `postOrder` / wallet.
    fn apply_buy_with_total_usdc(&mut self, qty: f64, total_usdc: f64) -> f64 {
        if !qty.is_finite() || qty <= 0.0 || !total_usdc.is_finite() || total_usdc <= 0.0 {
            return 0.0;
        }
        let new_total = self.shares + qty;
        if new_total.abs() < 1e-9 {
            *self = Default::default();
            return 0.0;
        }
        self.avg_entry = (self.shares * self.avg_entry + total_usdc) / new_total;
        self.shares = new_total;
        0.0
    }
}

/// One row in the Open orders panel (current market only).
#[derive(Debug, Clone)]
pub struct OpenOrderRow {
    pub side: Side,
    pub outcome: Outcome,
    pub price: f64,
    /// Unfilled size (shares).
    pub remaining: f64,
}

#[derive(Debug, Clone)]
pub struct Fill {
    pub ts: DateTime<Utc>,
    pub side: Side,
    pub outcome: Outcome,
    pub qty: f64,
    pub price: f64,
    pub realized: f64, // only non-zero when the fill closes part of a position
    /// CLOB `Trade.id` when known — REST bootstrap / user WS (dedupe); absent for `OrderAck`-only rows.
    pub clob_trade_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    EditSize,
    EditPrice,
    LimitModal {
        outcome: Outcome,
        side: Side,
        field: LimitField,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitField {
    Price,
    Size,
}

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

/// Pre-computed sentiment direction for the header — updated in `apply()`, read by `draw()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentimentDir {
    Up,
    Down,
    Neutral,
    Unknown,
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

pub fn manual_signal_label_to_buy_outcome(label: ManualSignalLabel) -> Option<Outcome> {
    match label {
        ManualSignalLabel::StrongUp => Some(Outcome::Up),
        ManualSignalLabel::StrongDown => Some(Outcome::Down),
        ManualSignalLabel::NoTrade | ManualSignalLabel::Watch => None,
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
    for p in &state.pending_trailing_sells {
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
    pub wizard_rows: Vec<SeriesRow>,
    pub wizard_list_idx: usize,
    /// Selected line in timeframe list (0 = 5m, 1 = 15m).
    pub wizard_tf_idx: usize,
    /// Last load error (wizard still usable via fallback list).
    pub wizard_series_error: Option<String>,

    pub market: Option<ActiveMarket>,
    /// Chainlink spot (same stream as market resolution for crypto up/down).
    pub spot_price: Option<f64>,
    pub spot_price_ts: Option<DateTime<Utc>>,
    /// `arc` of the same profile passed at [`AppEvent::StartTrading`].
    pub market_profile: Option<Arc<MarketProfile>>,

    // One book per outcome token
    /// Shared with [`Self::watched_books`] when the same token is both on-screen and trail-watched.
    pub book_up: Option<Arc<BookSnapshot>>,
    pub book_down: Option<Arc<BookSnapshot>>,

    pub position_up: Position,
    pub position_down: Position,
    pub realized_pnl: f64,

    /// Net long shares on UP token from FAK/ack path + resync on `PositionsLoaded` (for market SELL sizing).
    pub fak_net_up: f64,
    /// Net long shares on DOWN token (same).
    pub fak_net_down: f64,

    pub fills: VecDeque<Fill>,
    /// Trade IDs already represented in positions / fills. This is the final in-state guard for
    /// user-channel MINED -> CONFIRMED replays; `UserTradeSync` is a cross-task prefilter.
    seen_fill_trade_ids: HashSet<String>,
    /// Resting limit orders on the active market (from CLOB).
    pub open_orders: Vec<OpenOrderRow>,
    pub status_line: String,

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
    pub default_price: f64,
    pub size_input: String,  // buffer while editing size
    pub price_input: String, // buffer while editing default price
    pub limit_price_input: String,
    pub limit_size_input: String,

    pub input_mode: InputMode,

    /// When set, a bottom-right toast is drawn; keys are not blocked. Expires after [`ORDER_ERROR_TOAST_TTL`].
    pub order_error_toast: Option<OrderErrorToast>,

    /// `f` — Solana USDC deposit address + QR (Bridge API).
    pub deposit_modal: Option<DepositModalPhase>,

    /// Net UP vs DOWN after per-wallet “max of UP/DOWN position” (see `fetch_top_holders_amount_sums`); header Sentiment.
    pub top_holders_up_sum: Option<f64>,
    pub top_holders_down_sum: Option<f64>,

    /// Pre-computed from CLOB mid + top-holder sums; updated on Book and TopHoldersSentiment events.
    pub cached_sentiment: SentimentDir,
    /// Rolling 60s traded notional (`price * size`) from public prints + optional REST seed.
    pub cached_market_activity_notional: f64,
    /// Updated once per second in the Tick handler; avoids `Utc::now()` in the draw path.
    pub cached_countdown_secs: Option<i64>,
    /// `”{asset}/USD”` label, set once on StartTrading.
    pub cached_pair_label: String,

    /// Deduplication between `OrderAck` and user-channel `trade` (see `feeds::user_trade_sync`).
    pub user_trade_sync: Arc<crate::feeds::user_trade_sync::UserTradeSync>,

    /// Trailing stops keyed by CLOB outcome `token_id` (supports multiple markets at once).
    pub trailing: HashMap<String, TrailingSession>,
    /// Buy registered; waiting for **best bid** to reach entry × (1 + TP bps) before arming the trail.
    pub pending_trail_arms: HashMap<String, PendingTrailArm>,
    /// Books for tokens that are not the active UI pair but still have trailing / pending arms.
    pub watched_books: HashMap<String, Arc<BookSnapshot>>,
    /// FAK SELLs queued: priced on dispatch (empty book → stays queued until a later book tick).
    /// FIFO per enqueue; the same `token_id` is not enqueued while already queued or in-flight.
    pub pending_trailing_sells: VecDeque<TrailingExit>,
    /// `token_id`s with a `run_trailing_exit_fak_sell` task in progress; capped by
    /// [`TRAILING_SELL_MAX_PARALLEL`].
    pub trailing_sell_in_flight: HashSet<String>,
    /// `token_id`s with a stop-loss GTD SELL task in progress; prevents duplicate submits on
    /// repeated book ticks while the CLOB order is being placed.
    pub stop_loss_sell_in_flight: HashSet<String>,
    /// Residual trails re-armed after a partial trailing SELL; blocks duplicate triggers while
    /// delayed fills for the same CLOB order can still arrive.
    pub trailing_sell_partial_fill_grace_until: HashMap<String, Instant>,
    /// Cached result of `collect_book_watch_token_ids` — updated on every `apply()` call so
    /// `send_book_watch_if_changed` in `main.rs` can read it without recomputing.
    pub cached_book_watch_tokens: Vec<String>,
    book_watch_tokens_dirty: bool,
    /// Last [`AppEvent::MarketRoll`] — arms trailing on **maker** user-channel BUY fills (resting limits).
    pub buy_trail_bps: u32,
    pub buy_trail_activation_bps: u32,
    /// Maker BUY fill on user WS (resting limit) should run fixed take-profit in `main` after `apply`.
    pub pending_ws_take_profit: Option<(Outcome, f64)>,
    pub autotrading_enabled: bool,
    pub autotrading_max_positions: usize,
    pub autotrading_open: HashMap<String, f64>,
    pub autotrading_in_flight: HashSet<String>,
    pub autotrading_resting_buy_orders: HashMap<String, String>,
    /// Shared manual/automatic redeem-all gate; prevents overlapping relayer submissions.
    pub redeem_in_flight: bool,
    /// Last redeem-all task start time, used to throttle automatic and manual redeems together.
    pub last_redeem_started_at: Option<Instant>,
    /// Scales required spot gap for manual `STRONG` signal (from `STRATEGY_STRONG_GAP_MULT`).
    pub strategy_strong_gap_mult: f64,
    /// Scales max spread gate for strategy book usability (`STRATEGY_MAX_SPREAD_MULT`).
    pub strategy_max_spread_mult: f64,
    /// Minimum top-of-book ask size for strategy (`STRATEGY_MIN_TOP_ASK_SHARES`).
    pub strategy_min_top_ask_shares: f64,
    /// Watch threshold as fraction of strong gap (`STRATEGY_WATCH_RATIO`).
    pub strategy_watch_ratio: f64,

    /// Optional JSONL session logger ([`crate::round_log::RoundLogHandle`]).
    pub round_log: Option<std::sync::Arc<crate::round_log::RoundLogHandle>>,

    /// Directory for optional `{crypto}_{time_window}.json` signal model overrides ([`crate::signal`]).
    pub signal_model_dir: PathBuf,

    traded_activity: RollingTradedNotional,
}

impl AppState {
    pub fn new(
        default_size_usdc: f64,
        default_price: f64,
        user_trade_sync: Arc<crate::feeds::user_trade_sync::UserTradeSync>,
    ) -> Self {
        Self {
            ui_phase: UiPhase::WizardLoading,
            wizard_rows: Vec::new(),
            wizard_list_idx: 0,
            wizard_tf_idx: 0,
            wizard_series_error: None,
            market: None,
            spot_price: None,
            spot_price_ts: None,
            market_profile: None,
            book_up: None,
            book_down: None,
            position_up: Default::default(),
            position_down: Default::default(),
            realized_pnl: 0.0,
            fak_net_up: 0.0,
            fak_net_down: 0.0,
            fills: VecDeque::with_capacity(64),
            seen_fill_trade_ids: HashSet::new(),
            open_orders: Vec::new(),
            status_line: "Loading Polymarket markets…".into(),
            collateral_cash_usdc: None,
            collateral_claimable_usdc: None,
            latched_price_to_beat: None,
            default_size_usdc,
            default_price,
            size_input: format!("{default_size_usdc:.2}"),
            price_input: format!("{default_price:.2}"),
            limit_price_input: String::new(),
            limit_size_input: String::new(),
            input_mode: InputMode::Normal,
            order_error_toast: None,
            deposit_modal: None,
            top_holders_up_sum: None,
            top_holders_down_sum: None,
            cached_sentiment: SentimentDir::Unknown,
            cached_market_activity_notional: 0.0,
            cached_countdown_secs: None,
            cached_pair_label: "\u{2014}/USD".to_string(), // "—/USD"
            user_trade_sync,
            trailing: HashMap::new(),
            pending_trail_arms: HashMap::new(),
            watched_books: HashMap::new(),
            pending_trailing_sells: VecDeque::new(),
            trailing_sell_in_flight: HashSet::new(),
            stop_loss_sell_in_flight: HashSet::new(),
            trailing_sell_partial_fill_grace_until: HashMap::new(),
            cached_book_watch_tokens: Vec::new(),
            book_watch_tokens_dirty: false,
            buy_trail_bps: 0,
            buy_trail_activation_bps: 0,
            pending_ws_take_profit: None,
            autotrading_enabled: false,
            autotrading_max_positions: 1,
            autotrading_open: HashMap::new(),
            autotrading_in_flight: HashSet::new(),
            autotrading_resting_buy_orders: HashMap::new(),
            redeem_in_flight: false,
            last_redeem_started_at: None,
            strategy_strong_gap_mult: 1.0,
            strategy_max_spread_mult: 1.0,
            strategy_min_top_ask_shares: 5.0,
            strategy_watch_ratio: 0.60,
            round_log: None,
            signal_model_dir: PathBuf::from("./models"),
            traded_activity: RollingTradedNotional::new(),
        }
    }

    /// Clears and returns a pending fixed take-profit request from a **maker** user-channel BUY fill.
    pub fn take_pending_ws_take_profit(&mut self) -> Option<(Outcome, f64)> {
        self.pending_ws_take_profit.take()
    }

    /// Merge a pending trailing arm (same rules as [`AppEvent::RequestTrailingArm`]).
    fn merge_pending_trailing_buy_arm(
        &mut self,
        outcome: Outcome,
        entry_price: f64,
        plan_sell_shares: f64,
        token_id: String,
        trail_bps: u32,
        activation_bps: u32,
        market: ActiveMarket,
    ) {
        let token_id = canonical_clob_token_id(&token_id).into_owned();
        if trail_bps == 0 || !plan_sell_shares.is_finite() || plan_sell_shares <= 0.0 {
            return;
        }
        let tid = token_id.clone();
        let entry_key = pending_trail_map_key_for_asset(&self.pending_trail_arms, tid.as_str())
            .unwrap_or_else(|| tid.clone());
        match self.pending_trail_arms.entry(entry_key) {
            Entry::Occupied(mut e) => {
                let p = e.get_mut();
                let add_plan = plan_sell_shares;
                let add_entry = entry_price;
                if add_plan > 1e-9 && add_entry.is_finite() && add_entry > 0.0 {
                    let old_plan = p.plan_sell_shares;
                    let old_entry = p.entry_price;
                    let new_plan = old_plan + add_plan;
                    if new_plan > 1e-9 {
                        p.plan_sell_shares = new_plan;
                        if old_plan > 1e-9 && old_entry.is_finite() && old_entry > 0.0 {
                            p.entry_price =
                                (old_plan * old_entry + add_plan * add_entry) / new_plan;
                        } else {
                            p.entry_price = add_entry;
                        }
                    }
                }
            }
            Entry::Vacant(e) => {
                e.insert(PendingTrailArm {
                    market,
                    outcome,
                    entry_price,
                    plan_sell_shares,
                    token_id,
                    trail_bps,
                    activation_bps,
                });
            }
        }
        self.book_watch_tokens_dirty = true;
        self.status_line = format!(
            "trailing: {} pending — arm when bid ≥ entry×(1+{activation_bps} bps) (from position)",
            outcome.as_str()
        );
        self.try_promote_pending_trail_any(tid.as_str());
    }

    /// After `GET /data/trades` / [`AppEvent::PositionsLoaded`], register the same pending trail a
    /// live BUY would get, so restarts keep protection when `buy_trail_bps` is set on the last roll.
    ///
    /// Clears any existing trail/pending for each active leg first so repeated REST loads do not
    /// stack duplicate `plan_sell_shares`.
    fn rehydrate_trailing_from_positions_after_rest(&mut self) {
        if self.buy_trail_bps == 0 {
            return;
        }
        let Some(m) = self.market.clone() else {
            return;
        };
        for (outcome, token_raw) in [
            (Outcome::Up, m.up_token_id.as_str()),
            (Outcome::Down, m.down_token_id.as_str()),
        ] {
            let sh = self.position(outcome).shares;
            let ae = self.position(outcome).avg_entry;
            if sh <= 1e-9 || !ae.is_finite() || ae <= 0.0 || ae >= 0.99 {
                continue;
            }
            let tid = canonical_clob_token_id(token_raw).into_owned();
            self.clear_trailing_on_sell_token(tid.as_str());
            self.merge_pending_trailing_buy_arm(
                outcome,
                ae,
                sh,
                tid,
                self.buy_trail_bps,
                self.buy_trail_activation_bps,
                m.clone(),
            );
        }
    }

    /// True if `token_id` already has a queued trailing exit, in-flight FAK SELL, or active
    /// resting SELL row that has already locked the position for sale.
    fn trailing_sell_queued_or_in_flight(&self, token_id: &str) -> bool {
        debug_assert_eq!(
            token_id,
            canonical_clob_token_id(token_id).as_ref(),
            "token_id must be canonical before calling this"
        );
        self.pending_trailing_sells
            .iter()
            .any(|e| e.token_id == token_id)
            || self.trailing_sell_in_flight.contains(token_id)
            || self.stop_loss_sell_in_flight.contains(token_id)
            || self
                .trailing_sell_partial_fill_grace_until
                .get(token_id)
                .is_some_and(|until| Instant::now() < *until)
            || self
                .outcome_for_active_token(token_id)
                .is_some_and(|outcome| {
                    self.open_orders.iter().any(|o| {
                        o.side == Side::Sell
                            && o.outcome == outcome
                            && o.remaining > CLOB_ORDER_REMAINING_DUST
                    })
                })
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

    /// Fraction digits for Chainlink spot / open / delta in the header (XRP needs finer quotes).
    pub fn spot_usd_decimal_places(&self) -> usize {
        match self.market_profile.as_ref().map(|p| p.asset.label) {
            Some("XRP") => 4,
            _ => 2,
        }
    }

    pub fn book_for(&self, outcome: Outcome) -> Option<&BookSnapshot> {
        match outcome {
            Outcome::Up => self.book_up.as_deref(),
            Outcome::Down => self.book_down.as_deref(),
        }
    }

    /// Best ask on the outcome side we'd hit when buying YES(outcome).
    pub fn best_ask(&self, outcome: Outcome) -> Option<f64> {
        self.book_for(outcome)?.asks.first().map(|l| l.price)
    }
    /// Best bid on the outcome side we'd hit when selling YES(outcome).
    pub fn best_bid(&self, outcome: Outcome) -> Option<f64> {
        self.book_for(outcome)?.bids.first().map(|l| l.price)
    }

    pub fn stop_loss_sell_candidate(&self, stop_loss_bps: u32) -> Option<StopLossSell> {
        if stop_loss_bps == 0 {
            return None;
        }
        let market = self.market.as_ref()?;
        self.stop_loss_sell_candidate_for_outcome(
            market,
            Outcome::Up,
            market.up_token_id.as_str(),
            stop_loss_bps,
        )
        .or_else(|| {
            self.stop_loss_sell_candidate_for_outcome(
                market,
                Outcome::Down,
                market.down_token_id.as_str(),
                stop_loss_bps,
            )
        })
    }

    pub fn mark_stop_loss_sell_in_flight(&mut self, token_id: &str) {
        self.stop_loss_sell_in_flight
            .insert(canonical_clob_token_id(token_id).into_owned());
    }

    pub fn clear_stop_loss_sell_in_flight(&mut self, token_id: &str) {
        if !self.stop_loss_sell_in_flight.remove(token_id) {
            let canonical = canonical_clob_token_id(token_id);
            self.stop_loss_sell_in_flight.remove(canonical.as_ref());
        }
    }

    fn stop_loss_sell_candidate_for_outcome(
        &self,
        market: &ActiveMarket,
        outcome: Outcome,
        token_id: &str,
        stop_loss_bps: u32,
    ) -> Option<StopLossSell> {
        if self
            .stop_loss_sell_in_flight
            .iter()
            .any(|id| clob_asset_ids_match(id, token_id))
        {
            return None;
        }

        let current_price = self.best_bid(outcome)?;
        let reference_price = self.stop_loss_reference_price(outcome)?;
        if !stop_loss_triggered(reference_price, current_price, stop_loss_bps) {
            return None;
        }

        let shares = self.stop_loss_inventory_shares(outcome);
        if shares + 1e-9 < MIN_LIMIT_ORDER_SHARES {
            return None;
        }

        Some(StopLossSell {
            token_id: token_id.to_string(),
            market: market.clone(),
            outcome,
            shares,
            reference_price,
            current_price,
            limit_price: stop_loss_sell_limit_price(current_price),
        })
    }

    fn stop_loss_reference_price(&self, outcome: Outcome) -> Option<f64> {
        self.fills
            .iter()
            .filter(|f| f.side == Side::Buy && f.outcome == outcome)
            .map(|f| f.price)
            .filter(|p| p.is_finite() && *p > 0.0)
            .max_by(|a, b| a.total_cmp(b))
            .or_else(|| {
                let avg = self.position(outcome).avg_entry;
                (avg.is_finite() && avg > 0.0).then_some(avg)
            })
    }

    fn stop_loss_inventory_shares(&self, outcome: Outcome) -> f64 {
        let held = self.position(outcome).shares.max(0.0);
        let fill_net = net_shares_from_fills(&self.fills, outcome).max(0.0);
        let fak_net = match outcome {
            Outcome::Up => self.fak_net_up,
            Outcome::Down => self.fak_net_down,
        }
        .max(0.0);
        held.max(fill_net).max(fak_net)
    }

    /// Current mark (mid) price for an outcome, for unrealized PnL.
    pub fn mark(&self, outcome: Outcome) -> Option<f64> {
        let b = self.book_for(outcome)?;
        book_mid(b)
    }

    pub fn manual_signal_label(&self) -> ManualSignalLabel {
        self.signal_bundle().effective()
    }

    /// Rubric label plus optional model-driven label (see [`crate::signal`]).
    pub fn signal_bundle(&self) -> crate::signal::SignalBundle {
        crate::signal::evaluate(
            &self.signal_model_dir,
            self.market_profile
                .as_ref()
                .map(std::sync::Arc::as_ref),
            &self.manual_signal_input_snapshot(),
        )
    }

    /// Snapshot of inputs passed to [`evaluate_manual_signal`] (for JSONL `snap` replay).
    pub fn manual_signal_input_snapshot(&self) -> ManualSignalInput {
        ManualSignalInput {
            spot_price: self.spot_price,
            price_to_beat: self.price_to_beat(),
            up: self.manual_signal_book_side(Outcome::Up),
            down: self.manual_signal_book_side(Outcome::Down),
            seconds_to_close: self.cached_countdown_secs.or_else(|| {
                self.market
                    .as_ref()
                    .map(|m| (m.closes_at - Utc::now()).num_seconds().max(0))
            }),
            window_secs: self.manual_signal_window_secs(),
            sentiment: match self.cached_sentiment {
                SentimentDir::Up => ManualSignalSentiment::Up,
                SentimentDir::Down => ManualSignalSentiment::Down,
                SentimentDir::Neutral => ManualSignalSentiment::Neutral,
                SentimentDir::Unknown => ManualSignalSentiment::Unknown,
            },
            activity_notional_60s: self.cached_market_activity_notional,
            strong_gap_mult: self.strategy_strong_gap_mult,
            max_spread_mult: self.strategy_max_spread_mult,
            min_top_ask_shares: self.strategy_min_top_ask_shares,
            watch_ratio: self.strategy_watch_ratio,
        }
    }

    fn maybe_log_round_fill(&self, fill: &Fill) {
        let Some(ref rl) = self.round_log else {
            return;
        };
        if !rl.log_fills_enabled() {
            return;
        }
        let cid = self.market.as_ref().map(|m| m.condition_id.as_str());
        rl.log_fill(
            cid,
            fill.ts,
            fill.side,
            fill.outcome,
            fill.qty,
            fill.price,
            fill.realized,
            fill.clob_trade_id.clone(),
        );
    }

    pub fn manual_signal_buy_outcome(&self) -> Option<Outcome> {
        manual_signal_label_to_buy_outcome(self.manual_signal_label())
    }

    pub fn autotrading_reserved_count(&self) -> usize {
        let mut tokens = HashSet::new();
        for (token_id, qty) in &self.autotrading_open {
            if qty.is_finite() && *qty > AUTOTRADING_POSITION_DUST {
                tokens.insert(token_id.as_str());
            }
        }
        for token_id in &self.autotrading_in_flight {
            tokens.insert(token_id.as_str());
        }
        tokens.len()
    }

    pub fn autotrading_can_buy(&self, token_id: &str) -> bool {
        !self.autotrading_in_flight.contains(token_id)
            && !self
                .autotrading_open
                .get(token_id)
                .is_some_and(|qty| qty.is_finite() && *qty > AUTOTRADING_POSITION_DUST)
            && self.autotrading_reserved_count() < self.autotrading_max_positions.max(1)
    }

    pub fn autotrading_mark_in_flight(&mut self, token_id: String) -> bool {
        if !self.autotrading_can_buy(&token_id) {
            return false;
        }
        self.autotrading_in_flight.insert(token_id)
    }

    pub fn autotrading_clear_in_flight(&mut self, token_id: &str) {
        self.autotrading_in_flight.remove(token_id);
    }

    pub fn autotrading_track_resting_buy_order(&mut self, order_id: &str, token_id: &str) {
        let order_id = norm_order_id_key(order_id);
        if order_id.is_empty() {
            return;
        }
        let token_id = canonical_clob_token_id(token_id).into_owned();
        self.autotrading_resting_buy_orders
            .insert(order_id, token_id);
    }

    pub fn autotrading_clear_resting_buy_order(&mut self, order_id: &str) -> bool {
        let order_id = norm_order_id_key(order_id);
        let Some(token_id) = self.autotrading_resting_buy_orders.remove(&order_id) else {
            return false;
        };
        self.autotrading_clear_in_flight(&token_id);
        true
    }

    fn autotrading_clear_resting_buy_orders_for_token(&mut self, token_id: &str) {
        let token_id = canonical_clob_token_id(token_id);
        self.autotrading_resting_buy_orders
            .retain(|_, tracked_token| tracked_token != token_id.as_ref());
    }

    pub fn redeem_mark_in_flight(&mut self, now: Instant, min_interval: Duration) -> bool {
        if self.redeem_in_flight
            || self.last_redeem_started_at.is_some_and(|started| {
                match now.checked_duration_since(started) {
                    Some(elapsed) => elapsed < min_interval,
                    None => true,
                }
            })
        {
            return false;
        }
        self.redeem_in_flight = true;
        self.last_redeem_started_at = Some(now);
        true
    }

    pub fn redeem_clear_in_flight(&mut self) {
        self.redeem_in_flight = false;
    }

    pub fn autotrading_apply_fill(&mut self, token_id: &str, side: Side, qty: f64) {
        if !qty.is_finite() || qty <= AUTOTRADING_POSITION_DUST {
            return;
        }
        let token_id = canonical_clob_token_id(token_id).into_owned();
        match side {
            Side::Buy => {
                self.autotrading_clear_in_flight(&token_id);
                self.autotrading_clear_resting_buy_orders_for_token(&token_id);
                *self.autotrading_open.entry(token_id).or_insert(0.0) += qty;
            }
            Side::Sell => {
                if let Entry::Occupied(mut e) = self.autotrading_open.entry(token_id) {
                    let remaining = (*e.get() - qty).max(0.0);
                    if remaining <= AUTOTRADING_POSITION_DUST {
                        e.remove();
                    } else {
                        *e.get_mut() = remaining;
                    }
                }
            }
        }
    }

    fn claim_fill_trade_id(&mut self, clob_trade_id: Option<&str>) -> bool {
        let Some(key) = fill_trade_id_key(clob_trade_id) else {
            return true;
        };
        self.seen_fill_trade_ids.insert(key)
    }

    fn manual_signal_book_side(&self, outcome: Outcome) -> ManualSignalBookSide {
        let book = self.book_for(outcome);
        ManualSignalBookSide {
            best_bid: book.and_then(|b| b.bids.first().map(|l| l.price)),
            best_ask: book.and_then(|b| b.asks.first().map(|l| l.price)),
            best_ask_size: book.and_then(|b| b.asks.first().map(|l| l.size)),
        }
    }

    fn manual_signal_window_secs(&self) -> Option<i64> {
        self.market_profile
            .as_ref()
            .and_then(|p| p.timeframe.window_sec_rolling())
            .or_else(|| {
                self.market
                    .as_ref()
                    .map(|m| (m.closes_at - m.opens_at).num_seconds())
                    .filter(|s| *s > 0)
            })
    }

    /// Best bid for `token_id` (UI or watched book).
    pub fn best_bid_for_token(&self, token_id: &str) -> Option<f64> {
        let b = self.book_snapshot_for_token(token_id)?;
        b.bids.first().map(|l| l.price)
    }

    fn book_snapshot_for_token(&self, token_id: &str) -> Option<&BookSnapshot> {
        if let Some(m) = &self.market {
            if token_id == m.up_token_id.as_str() {
                return self.book_up.as_deref();
            }
            if token_id == m.down_token_id.as_str() {
                return self.book_down.as_deref();
            }
        }
        self.watched_books
            .get(token_id)
            .or_else(|| {
                let c = canonical_clob_token_id(token_id);
                if c.as_ref() != token_id {
                    self.watched_books.get(c.as_ref())
                } else {
                    None
                }
            })
            .map(|b| b.as_ref())
    }

    /// If `token_id` belongs to the active UI market, return its UP/DOWN side.
    ///
    /// Uses [`clob_asset_ids_match`] so decimal / `0x` hex forms agree with user-channel and book
    /// paths (strict `==` alone left live `position_*` updated while trailing still used stale
    /// `tracked_shares`, causing SELLs after a manual exit).
    pub fn outcome_for_active_token(&self, token_id: &str) -> Option<Outcome> {
        let m = self.market.as_ref()?;
        if token_id == m.up_token_id.as_str()
            || clob_asset_ids_match(token_id, m.up_token_id.as_str())
        {
            Some(Outcome::Up)
        } else if token_id == m.down_token_id.as_str()
            || clob_asset_ids_match(token_id, m.down_token_id.as_str())
        {
            Some(Outcome::Down)
        } else {
            None
        }
    }

    /// Long shares available for trailing sizing: live REST/UI position when the trail’s market is
    /// the active one; else active-token mapping; else `sess.tracked_shares` for background tails.
    fn trail_live_shares(&self, sess: &TrailingSession) -> f64 {
        if self
            .market
            .as_ref()
            .is_some_and(|m| m.condition_id == sess.market.condition_id)
        {
            return self.position(sess.outcome).shares.max(0.0);
        }
        if let Some(oc) = self.outcome_for_active_token(sess.token_id.as_str()) {
            return self.position(oc).shares.max(0.0);
        }
        sess.tracked_shares.max(0.0)
    }

    /// Cost basis for status / trailing exit checks (same rules as `apply_trailing_book_tick`).
    pub fn trailing_session_entry_px(&self, token_id: &str) -> Option<f64> {
        let map_key = trailing_map_key_for_asset(&self.trailing, token_id)?;
        let sess = self.trailing.get(&map_key)?;
        let entry_px = if self
            .market
            .as_ref()
            .is_some_and(|m| m.condition_id == sess.market.condition_id)
        {
            let pos = self.position(sess.outcome);
            if pos.shares > 1e-9 && pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                pos.avg_entry
            } else {
                sess.stop.entry_price()
            }
        } else if let Some(oc) = self.outcome_for_active_token(map_key.as_str()) {
            let pos = self.position(oc);
            if pos.shares > 1e-9 && pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                pos.avg_entry
            } else {
                sess.stop.entry_price()
            }
        } else {
            sess.stop.entry_price()
        };
        (entry_px.is_finite() && entry_px > 0.0).then_some(entry_px)
    }

    /// Entry for a queued trailing FAK sell: armed session if present, else live position on the exit market.
    pub fn trailing_exit_entry_for_dispatch(&self, ex: &TrailingExit) -> Option<f64> {
        if let Some(px) = self.trailing_session_entry_px(ex.token_id.as_str()) {
            return Some(px);
        }
        if self
            .market
            .as_ref()
            .is_some_and(|m| m.condition_id == ex.market.condition_id)
        {
            let pos = self.position(ex.outcome);
            if pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                return Some(pos.avg_entry);
            }
        }
        None
    }

    /// Drop trailing / pending-arm rows when REST replay shows no position on that leg (manual sell
    /// or fills missed on the user channel).
    fn sync_trailing_inventory_with_positions(&mut self) {
        let Some(active_cid) = self.market.as_ref().map(|m| m.condition_id.clone()) else {
            return;
        };
        let clear_trailing: Vec<String> = self
            .trailing
            .iter()
            .filter(|(_, sess)| {
                sess.market.condition_id == active_cid && self.position(sess.outcome).shares <= 1e-9
            })
            .map(|(k, _)| k.clone())
            .collect();
        for tid in clear_trailing {
            self.clear_trailing_on_sell_token(&tid);
        }
        let clear_pending: Vec<String> = self
            .pending_trail_arms
            .iter()
            .filter(|(_, p)| {
                p.market.condition_id == active_cid && self.position(p.outcome).shares <= 1e-9
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in clear_pending {
            self.pending_trail_arms.remove(&k);
        }
    }

    /// Number of trailing / pending-trail sessions not on the current `market` (if any).
    pub fn background_trail_count(&self) -> usize {
        let Some(m) = self.market.as_ref() else {
            return self.trailing.len() + self.pending_trail_arms.len();
        };
        let mut n = 0;
        for tid in self.trailing.keys() {
            if tid != &m.up_token_id && tid != &m.down_token_id {
                n += 1;
            }
        }
        for tid in self.pending_trail_arms.keys() {
            if tid != &m.up_token_id && tid != &m.down_token_id {
                n += 1;
            }
        }
        n
    }

    pub fn position(&self, outcome: Outcome) -> &Position {
        match outcome {
            Outcome::Up => &self.position_up,
            Outcome::Down => &self.position_down,
        }
    }
    fn position_mut(&mut self, outcome: Outcome) -> &mut Position {
        match outcome {
            Outcome::Up => &mut self.position_up,
            Outcome::Down => &mut self.position_down,
        }
    }

    /// If session [`Self::fills`] imply **more** gross long shares than [`Position::shares`], bump
    /// inventory to match (and repair `avg_entry` when missing). Does **not** shrink the position
    /// (SELL path stays authoritative).
    fn reconcile_position_shares_with_fill_deque(&mut self, oc: Outcome) {
        let net = net_shares_from_fills(&self.fills, oc);
        if !net.is_finite() || net < 1e-12 {
            return;
        }
        let cur_shares = self.position(oc).shares;
        let tol = f64::max(1e-6, 1e-4 * net.abs().max(1.0));
        if net <= cur_shares + tol {
            return;
        }
        let vwap = vwap_buy_price_from_fills_for_outcome(&self.fills, oc);
        {
            let p = self.position_mut(oc);
            p.shares = net.max(0.0);
            if !p.avg_entry.is_finite() || p.avg_entry <= 1e-12 {
                if vwap.is_finite() && vwap > 1e-12 {
                    p.avg_entry = vwap;
                }
            }
        }
        let after = self.position(oc).shares;
        let fk = self.fak_net_mut(oc);
        if after > *fk {
            *fk = after;
        }
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

    pub fn current_size(&self) -> f64 {
        self.size_input.parse().unwrap_or(self.default_size_usdc)
    }

    pub fn current_price(&self) -> f64 {
        self.price_input
            .parse::<f64>()
            .ok()
            .filter(|p| (0.01..=0.99).contains(p))
            .unwrap_or(self.default_price)
    }

    /// Drop a trailing plan when the user (or the exchange) reduces position via a SELL fill.
    fn clear_trailing_on_sell_token(&mut self, token_id: &str) {
        // All map/set keys are stored in canonical decimal form; canonicalize once so
        // every comparison below is a plain string equality — no U256 bignum fallback.
        let canonical = canonical_clob_token_id(token_id);
        let tid = canonical.as_ref();
        self.trailing.retain(|k, _| k != tid);
        self.pending_trail_arms.retain(|k, _| k != tid);
        self.watched_books.retain(|k, _| k != tid);
        self.pending_trailing_sells.retain(|e| e.token_id != tid);
        self.trailing_sell_in_flight.retain(|k| k != tid);
        self.trailing_sell_partial_fill_grace_until.remove(tid);
        self.book_watch_tokens_dirty = true;
    }

    /// Remove armed / pending trail rows for `token_id` only (no SELL queue / watched-book purge).
    fn remove_trailing_and_pending_arm_for_token(&mut self, token_id: &str) {
        let tid = canonical_clob_token_id(token_id);
        let t = tid.as_ref();
        if let Some(k) = trailing_map_key_for_asset(&self.trailing, t) {
            self.trailing.remove(&k);
            self.book_watch_tokens_dirty = true;
        }
        if let Some(k) = pending_trail_map_key_for_asset(&self.pending_trail_arms, t) {
            self.pending_trail_arms.remove(&k);
            self.book_watch_tokens_dirty = true;
        }
        self.trailing_sell_partial_fill_grace_until.remove(t);
    }

    /// After a BUY fill on the **active** UI market, unregister any trail/pending for that token and
    /// re-register from live `position` (total shares + `avg_entry`) so partial fills cannot leave
    /// stale `plan_sell_shares` or a mis-merged ratchet on [`TrailingStop`].
    fn refresh_buy_trailing_from_position(&mut self, outcome: Outcome, token_id: &str) {
        if self.buy_trail_bps == 0 {
            return;
        }
        if self.outcome_for_active_token(token_id) != Some(outcome) {
            return;
        }
        let had_trailing = trailing_map_key_for_asset(&self.trailing, token_id).is_some();
        let had_pending =
            pending_trail_map_key_for_asset(&self.pending_trail_arms, token_id).is_some();
        let pos_shares = self.position(outcome).shares;
        let pos_avg = self.position(outcome).avg_entry;
        if !had_trailing && !had_pending && pos_shares <= 1e-9 {
            return;
        }
        let Some(market) = self.market.clone() else {
            return;
        };
        let tid = canonical_clob_token_id(token_id).into_owned();
        self.remove_trailing_and_pending_arm_for_token(tid.as_str());
        if pos_shares <= 1e-9 || !pos_avg.is_finite() || pos_avg <= 0.0 || pos_avg >= 0.99 {
            return;
        }
        self.merge_pending_trailing_buy_arm(
            outcome,
            pos_avg,
            pos_shares,
            tid,
            self.buy_trail_bps,
            self.buy_trail_activation_bps,
            market,
        );
    }

    /// A trailing FAK SELL can partially fill. Re-arm the unsold planned shares while the exit task
    /// is still in flight; ordinary/user SELLs still clear trailing as before.
    fn rearm_trailing_after_partial_sell(&mut self, token_id: &str, sold_qty: f64) -> bool {
        let Some(map_key) = trailing_map_key_for_asset(&self.trailing, token_id) else {
            return false;
        };
        let Some(sess) = self.trailing.remove(&map_key) else {
            return false;
        };
        self.book_watch_tokens_dirty = true;

        let live_shares = self.trail_live_shares(&sess);
        let remaining_plan = (sess.plan_sell_shares - sold_qty).max(0.0).min(live_shares);
        if live_shares <= CLOB_ORDER_REMAINING_DUST
            || remaining_plan <= CLOB_ORDER_REMAINING_DUST
            || sess.trail_bps == 0
        {
            return false;
        }

        let entry = if self
            .market
            .as_ref()
            .is_some_and(|m| m.condition_id == sess.market.condition_id)
        {
            let pos = self.position(sess.outcome);
            if pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                pos.avg_entry
            } else {
                sess.stop.entry_price()
            }
        } else if let Some(oc) = self.outcome_for_active_token(&sess.token_id) {
            let pos = self.position(oc);
            if pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                pos.avg_entry
            } else {
                sess.stop.entry_price()
            }
        } else {
            sess.stop.entry_price()
        };

        let token_id_for_grace = sess.token_id.clone();
        self.install_trailing_session(
            sess.market,
            sess.outcome,
            entry,
            remaining_plan,
            sess.token_id,
            sess.trail_bps,
            live_shares.max(0.0),
        );
        self.trailing_sell_partial_fill_grace_until.insert(
            token_id_for_grace,
            Instant::now() + TRAILING_PARTIAL_FILL_GRACE,
        );
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn install_trailing_session(
        &mut self,
        market: ActiveMarket,
        outcome: Outcome,
        entry_price: f64,
        plan_sell_shares: f64,
        token_id: String,
        trail_bps: u32,
        tracked_shares: f64,
    ) {
        let token_id = canonical_clob_token_id(&token_id).into_owned();
        if trail_bps == 0 || !plan_sell_shares.is_finite() || plan_sell_shares <= 0.0 {
            return;
        }
        // Second (or later) market buy on the same `token_id` promotes or installs again; merge
        // with the existing session so `plan_sell_shares` caps the combined trailed inventory.
        let (plan_sell_shares, tracked_shares) =
            match trailing_map_key_for_asset(&self.trailing, token_id.as_str())
                .and_then(|k| self.trailing.remove(&k))
            {
                Some(prev) => (
                    plan_sell_shares + prev.plan_sell_shares,
                    tracked_shares.max(prev.tracked_shares),
                ),
                None => (plan_sell_shares, tracked_shares),
            };
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
        self.book_watch_tokens_dirty = true;
        self.status_line = format!(
            "trailing {out} — {trail_bps} bps trail on bid, SELL up to {plan_sell_shares:.2} sh",
            out = outcome.as_str()
        );
    }

    /// If a pending trail for `token_id` is satisfied by the book and position, move into
    /// [`Self::trailing`]. Entry for the threshold is live `avg_entry` when the outcome has
    /// shares on the **active** UI market for that token, else the pending REST/fill `entry_price`.
    fn try_promote_pending_trail_token(&mut self, token_id: &str) {
        let map_key = match pending_trail_map_key_for_asset(&self.pending_trail_arms, token_id) {
            Some(k) => k,
            None => return,
        };
        let p0 = match self.pending_trail_arms.get(&map_key) {
            Some(n) => n.clone(),
            None => return,
        };
        let bps = p0.activation_bps as f64 / 10_000.0;
        let Some(bid) = self.best_bid_for_token(token_id) else {
            return;
        };
        let entry = if self
            .market
            .as_ref()
            .is_some_and(|m| m.condition_id == p0.market.condition_id)
        {
            let pos = self.position(p0.outcome);
            if pos.shares > 1e-9 && pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                pos.avg_entry
            } else {
                p0.entry_price
            }
        } else if let Some(oc) = self.outcome_for_active_token(token_id) {
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
        let min_bid = entry * (1.0 + bps);
        if bid + 1e-12 < min_bid {
            return;
        }
        self.pending_trail_arms.remove(&map_key);
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

    fn apply_trailing_book_tick(&mut self, asset_id: &str, bid: f64) {
        // Hot path: find the session with a single direct lookup — no String allocation.
        // canonical_clob_token_id is only called when the direct hit misses (non-canonical form).
        let tick = {
            let Some(sess) = self
                .trailing
                .get(asset_id)
                .or_else(|| {
                    let c = canonical_clob_token_id(asset_id);
                    if c.as_ref() != asset_id {
                        self.trailing.get(c.as_ref())
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    self.trailing
                        .values()
                        .find(|s| clob_asset_ids_match(&s.token_id, asset_id))
                })
            else {
                return;
            };
            if self.trailing_sell_queued_or_in_flight(sess.token_id.as_str()) {
                return;
            }
            sess.stop.on_price(bid)
        };
        let TickOutcome::Triggered { .. } = tick else {
            return;
        };
        // Triggered (rare): look up key once; merge the two former post-trigger lookups into one.
        let Some(map_key) = trailing_map_key_for_asset(&self.trailing, asset_id) else {
            return;
        };
        let (token_id, market, outcome, plan, entry_px, live) = {
            let sess = self.trailing.get(&map_key).expect("trailing session");
            let entry_px = self
                .trailing_session_entry_px(sess.token_id.as_str())
                .unwrap_or_else(|| sess.stop.entry_price());
            let live = self.trail_live_shares(sess);
            (
                sess.token_id.clone(),
                sess.market.clone(),
                sess.outcome,
                sess.plan_sell_shares,
                entry_px,
                live,
            )
        };
        // Always queue on trip: the trail already decided to exit. A legacy `mid > entry` gate
        // became wrong when the feed switched from mid to best bid — bid at the stop is often
        // ≤ entry even for profitable round-trips (spread), which suppressed all SELLs.
        let sh = live.min(plan);
        if sh > CLOB_ORDER_REMAINING_DUST {
            if !self.trailing_sell_queued_or_in_flight(&token_id) {
                self.pending_trailing_sells.push_back(TrailingExit {
                    token_id,
                    market,
                    outcome,
                    sell_shares: sh,
                });
                self.book_watch_tokens_dirty = true;
            }
            self.status_line = format!(
                "trailing: {} tripped (bid {bid:.2}, entry {entry_px:.2}) — SELL {sh:.2} sh",
                outcome.as_str()
            );
        } else {
            self.remove_trailing_and_pending_arm_for_token(&token_id);
            self.status_line = format!(
                "trailing: {} tripped (bid {bid:.2}, entry {entry_px:.2}) — no shares to SELL",
                outcome.as_str()
            );
        }
    }

    /// Update [`TrailingSession::tracked_shares`] when fills arrive on a token that may not map
    /// to the active UI pair.
    fn bump_trailing_tracked_shares(&mut self, token_id: &str, side: Side, qty: f64) {
        let Some(map_key) = trailing_map_key_for_asset(&self.trailing, token_id) else {
            return;
        };
        if let Some(sess) = self.trailing.get_mut(&map_key) {
            match side {
                Side::Buy => sess.tracked_shares += qty,
                Side::Sell => sess.tracked_shares = (sess.tracked_shares - qty).max(0.0),
            }
        }
    }

    /// Reserved for PnL on non-UI legs; inventory is tracked via [`Self::bump_trailing_tracked_shares`].
    fn apply_background_trailing_fill(&mut self, _token_id: &str, _side: Side, _qty: f64) {}

    fn recompute_sentiment(&mut self) {
        const SENT_EPS: f64 = 1e-6;
        let m_up = self.mark(Outcome::Up);
        let m_down = self.mark(Outcome::Down);
        self.cached_sentiment = match (m_up, m_down) {
            (Some(u), Some(d)) if u > d + SENT_EPS => SentimentDir::Up,
            (Some(u), Some(d)) if d > u + SENT_EPS => SentimentDir::Down,
            (Some(_), Some(_)) => SentimentDir::Neutral,
            _ => match (self.top_holders_up_sum, self.top_holders_down_sum) {
                (Some(u), Some(d)) if u > d + SENT_EPS => SentimentDir::Up,
                (Some(u), Some(d)) if d > u + SENT_EPS => SentimentDir::Down,
                (Some(_), Some(_)) => SentimentDir::Neutral,
                _ => SentimentDir::Unknown,
            },
        };
    }

    fn refresh_market_activity_cache(&mut self) {
        let now = Utc::now().timestamp_millis();
        self.traded_activity.prune_against_wall_clock(now);
        let t = self.traded_activity.total();
        self.cached_market_activity_notional = if t.is_finite() { t } else { 0.0 };
    }

    fn token_watched_for_activity(&self, token: &str) -> bool {
        if self
            .cached_book_watch_tokens
            .iter()
            .any(|t| clob_asset_ids_match(t, token))
        {
            return true;
        }
        if let Some(m) = &self.market {
            return clob_asset_ids_match(token, m.up_token_id.as_str())
                || clob_asset_ids_match(token, m.down_token_id.as_str());
        }
        false
    }

    fn record_public_trade_activity(&mut self, p: crate::feeds::clob_ws::ClobTradePrint) {
        if self.token_watched_for_activity(p.asset_id.as_str()) {
            let n = p.price * p.size;
            self.traded_activity.record_trade(p.ts_ms, n);
        }
    }

    // ── Mutations ───────────────────────────────────────────────────
    pub async fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Tick => {
                if self
                    .order_error_toast
                    .as_ref()
                    .is_some_and(|t| Instant::now() >= t.until)
                {
                    self.order_error_toast = None;
                }
                self.cached_countdown_secs = self
                    .market
                    .as_ref()
                    .map(|m| (m.closes_at - Utc::now()).num_seconds().max(0));
                self.refresh_market_activity_cache();
            }
            AppEvent::Price(p) => {
                self.spot_price = Some(p.price);
                self.spot_price_ts =
                    chrono::DateTime::<Utc>::from_timestamp_millis(p.timestamp_ms as i64);
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
            AppEvent::Book(mut b) => {
                // clob_ws already emits canonical (all-decimal) IDs; skip the bignum
                // round-trip when the fast ASCII check confirms this.
                if !b.asset_id.bytes().all(|c| c.is_ascii_digit()) {
                    b.asset_id = canonical_clob_token_id(&b.asset_id).into_owned();
                }
                let snap = Arc::new(b);
                if let Some(m) = &self.market {
                    if clob_asset_ids_match(snap.asset_id.as_str(), m.up_token_id.as_str()) {
                        self.book_up = Some(Arc::clone(&snap));
                    } else if clob_asset_ids_match(snap.asset_id.as_str(), m.down_token_id.as_str())
                    {
                        self.book_down = Some(Arc::clone(&snap));
                    }
                }
                // Use the cached sorted watch list — binary search, no HashSet allocation.
                // cached_book_watch_tokens is rebuilt at the end of apply() to capture any
                // state changes made by try_promote_pending_trail_any / apply_trailing_book_tick.
                if self
                    .cached_book_watch_tokens
                    .binary_search(&snap.asset_id)
                    .is_ok()
                {
                    self.watched_books
                        .insert(snap.asset_id.clone(), Arc::clone(&snap));
                }

                self.try_promote_pending_trail_any(snap.asset_id.as_str());
                if let Some(bid) = self.best_bid_for_token(snap.asset_id.as_str()) {
                    self.apply_trailing_book_tick(snap.asset_id.as_str(), bid);
                }
                self.recompute_sentiment();
            }
            AppEvent::ClobPublicTrades(prints) => {
                for p in prints {
                    self.record_public_trade_activity(p);
                }
                self.refresh_market_activity_cache();
            }
            AppEvent::ActivityTradesSeed(prints) => {
                for p in prints {
                    if !self.token_watched_for_activity(p.asset_id.as_str()) {
                        continue;
                    }
                    let n = p.price * p.size;
                    self.traded_activity.record_trade(p.ts_ms, n);
                }
                self.refresh_market_activity_cache();
            }
            AppEvent::MarketRoll {
                market,
                buy_trail_bps,
                buy_trail_activation_bps,
            } => {
                // Fold open mark-to-market PnL into `realized_pnl` before we drop books /
                // positions. Otherwise `total_pnl` (rPnL + uPnL) would collapse to rPnL alone
                // across the roll even though economics were unchanged at the last mark.
                let roll_upnl =
                    self.unrealized_pnl(Outcome::Up) + self.unrealized_pnl(Outcome::Down);
                if roll_upnl.is_finite() {
                    self.realized_pnl += roll_upnl;
                }
                // Close any positions from the previous market — they'll resolve
                // via Polymarket and show up as realized once winnings redeem.
                // We keep realized_pnl but zero out live positions for the new market.
                // Return to normal keys (w/s/a/d/…) — otherwise `e` / limit modal survives a roll.
                // Deposit modal (`f`) stays open across rolls.
                self.input_mode = InputMode::Normal;
                self.order_error_toast = None;
                self.status_line = format!("New market: {}", market.question);
                self.latched_price_to_beat = None;
                self.market = Some(market);
                self.buy_trail_bps = buy_trail_bps;
                self.buy_trail_activation_bps = buy_trail_activation_bps;
                self.pending_ws_take_profit = None;
                self.book_up = None;
                self.book_down = None;
                self.position_up = Default::default();
                self.position_down = Default::default();
                self.fak_net_up = 0.0;
                self.fak_net_down = 0.0;
                self.fills.clear();
                self.seen_fill_trade_ids.clear();
                self.open_orders.clear();
                self.top_holders_up_sum = None;
                self.top_holders_down_sum = None;
                self.trailing.clear();
                self.pending_trail_arms.clear();
                self.pending_trailing_sells.clear();
                self.trailing_sell_in_flight.clear();
                self.stop_loss_sell_in_flight.clear();
                self.trailing_sell_partial_fill_grace_until.clear();
                self.watched_books.clear();
                self.autotrading_open.clear();
                self.autotrading_in_flight.clear();
                self.autotrading_resting_buy_orders.clear();
                self.cached_sentiment = SentimentDir::Unknown;
                self.cached_countdown_secs = None;
                self.traded_activity.clear();
                self.refresh_market_activity_cache();
                self.book_watch_tokens_dirty = true;
            }
            AppEvent::PriceToBeatRefresh {
                slug,
                price_to_beat,
            } => {
                if let Some(m) = &mut self.market {
                    if m.slug == slug {
                        if let Some(ptb) = price_to_beat {
                            m.price_to_beat = Some(ptb);
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
                if !fills_bootstrap.is_empty() {
                    let ids = fills_bootstrap
                        .iter()
                        .filter_map(|f| f.clob_trade_id.clone())
                        .filter(|s| !s.is_empty());
                    self.user_trade_sync.seed_seen_trades_from_rest(ids).await;
                    for f in fills_bootstrap {
                        if !self.claim_fill_trade_id(f.clob_trade_id.as_deref()) {
                            continue;
                        }
                        self.fills.push_back(f);
                    }
                    trim_fills_to_cap(&mut self.fills, 64);
                }
                // Only apply REST position if it carries at least as many shares as the
                // current in-memory state. The REST fetch is spawned at market roll and may
                // complete while WS fills have already updated the position (REST indexing
                // lags ~1-5 s). If REST has equal-or-more shares it is authoritative (full
                // trade-history VWAP); if it has fewer, WS already applied a fill that REST
                // hasn't indexed yet — keep the WS-updated position to avoid clobbering it.
                if position_up.shares >= self.position_up.shares {
                    self.position_up = position_up;
                }
                if position_down.shares >= self.position_down.shares {
                    self.position_down = position_down;
                }
                // Session fills: REST bootstrap above + user WS / OrderAck; cleared on MarketRoll.
                let nu = 0.0_f64;
                let nd = 0.0_f64;
                self.fak_net_up = self.position_up.shares.max(nu);
                self.fak_net_down = self.position_down.shares.max(nd);
                self.reconcile_position_shares_with_fill_deque(Outcome::Up);
                self.reconcile_position_shares_with_fill_deque(Outcome::Down);
                if refresh_status_line {
                    if self.position_up.shares <= 1e-9 && self.position_down.shares <= 1e-9 {
                        self.status_line = "No active CLOB positions on this market".into();
                    } else {
                        self.status_line = format!(
                            "Positions from CLOB — UP {:.2} @ {:.2} / DOWN {:.2} @ {:.2}",
                            self.position_up.shares,
                            self.position_up.avg_entry,
                            self.position_down.shares,
                            self.position_down.avg_entry,
                        );
                    }
                }
                if let Some(m) = self.market.clone() {
                    self.sync_trailing_inventory_with_positions();
                    self.rehydrate_trailing_from_positions_after_rest();
                    self.try_promote_pending_trail_any(m.up_token_id.as_str());
                }
                self.book_watch_tokens_dirty = true;
            }
            AppEvent::OpenOrdersLoaded { orders } => {
                self.open_orders = orders;
            }
            AppEvent::BalancePanelLoaded {
                cash_usdc,
                claimable_usdc,
                claimable_refreshed: _,
            } => {
                self.collateral_cash_usdc = Some(cash_usdc);
                self.collateral_claimable_usdc = Some(claimable_usdc);
            }
            AppEvent::TopHoldersSentiment { up_sum, down_sum } => {
                self.top_holders_up_sum = Some(up_sum);
                self.top_holders_down_sum = Some(down_sum);
                self.recompute_sentiment();
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
                from_maker_leg,
            } => {
                // Event-loop de-dupe guards — run here (sequentially with OrderAck) to close two
                // races that the WS-task pre-check in `before_ws_user_fill_apply` cannot prevent:
                //
                // 1. WS reconnect delivers same trade before `after_ws_user_fill_committed` adds
                //    the ID to `seen_trades`, so both `UserChannelFill` events pass the pre-check.
                // 2. `OrderAck` for the same fill is queued ahead of `UserChannelFill` and is
                //    processed first; `ack_wait` now holds the entry but the WS pre-check already
                //    returned `true` before that happened.
                if !self.claim_fill_trade_id(Some(&clob_trade_id)) {
                    return;
                }
                if self
                    .user_trade_sync
                    .fill_already_committed(&clob_trade_id)
                    .await
                    || self
                        .user_trade_sync
                        .ack_claimed_for_ws_fill(&order_leg_id, qty, price, &clob_trade_id)
                        .await
                {
                    return;
                }
                let token_id = canonical_clob_token_id(&token_id).into_owned();
                match side {
                    Side::Sell => {
                        self.autotrading_apply_fill(&token_id, side, qty);
                        self.clear_stop_loss_sell_in_flight(&token_id);
                    }
                    Side::Buy if from_maker_leg => {
                        // Resting auto-trading GTD: POST may be `live` with no fill ack — track
                        // inventory when the user channel reports a maker BUY leg (immediate fills
                        // use [`AppEvent::AutoTradingBuyDone`]).
                        self.autotrading_apply_fill(&token_id, side, qty);
                    }
                    _ => {}
                }
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
                    // Always record a Fills row for user-channel trades on the active market: the CLOB
                    // tape can show SELL on one outcome while the user's intent was a limit BUY on the
                    // other leg; suppressing "zero-inventory SELL" rows hid legitimate fills (see debug).
                    if qty > 1e-12 {
                        let fill = Fill {
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
                        };
                        self.maybe_log_round_fill(&fill);
                        self.fills.push_back(fill);
                        trim_fills_to_cap(&mut self.fills, 64);
                    }
                    self.reconcile_position_shares_with_fill_deque(ui_oc);
                    if matches!(side, Side::Buy)
                        && self.buy_trail_bps > 0
                        && qty > 1e-12
                        && price.is_finite()
                        && price > 0.0
                        && price < 0.99
                    {
                        self.refresh_buy_trailing_from_position(ui_oc, &token_id);
                    }
                    if matches!(side, Side::Buy)
                        && from_maker_leg
                        && self.buy_trail_bps == 0
                        && self.buy_trail_activation_bps > 0
                        && qty > 1e-12
                        && price.is_finite()
                        && price > 0.0
                        && price < 0.99
                    {
                        self.pending_ws_take_profit = Some((ui_oc, qty));
                    }
                    self.user_trade_sync
                        .after_ws_user_fill_committed(&clob_trade_id, &order_leg_id, qty, price)
                        .await;
                } else {
                    self.apply_background_trailing_fill(&token_id, side, qty);
                    self.user_trade_sync
                        .after_ws_user_fill_committed(&clob_trade_id, &order_leg_id, qty, price)
                        .await;
                }
                let skip_bump_and_trail_promote = matches!(side, Side::Buy)
                    && self.buy_trail_bps > 0
                    && self.outcome_for_active_token(&token_id).is_some();
                if !skip_bump_and_trail_promote {
                    self.bump_trailing_tracked_shares(&token_id, side, qty);
                }
                if side == Side::Sell {
                    let rearmed = self.trailing_sell_in_flight.contains(&token_id)
                        && self.rearm_trailing_after_partial_sell(&token_id, qty);
                    if !rearmed {
                        self.clear_trailing_on_sell_token(&token_id);
                    }
                } else if !skip_bump_and_trail_promote {
                    self.try_promote_pending_trail_any(token_id.as_str());
                }
                self.status_line = format!(
                    "{} {qty:.2} {} @ {price:.2} (WSS trade)",
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
                let token_id = canonical_clob_token_id(&token_id).into_owned();
                if side == Side::Sell {
                    self.autotrading_apply_fill(&token_id, side, qty);
                    self.clear_stop_loss_sell_in_flight(&token_id);
                }
                let mut do_pnl = true;
                if let Some(ref oid) = clob_order_id {
                    if !self
                        .user_trade_sync
                        .before_order_ack_apply(oid, qty, price)
                        .await
                    {
                        do_pnl = false;
                    }
                }
                if do_pnl {
                    let mut filled_oc: Option<Outcome> = None;
                    if let Some(ui_oc) = self.outcome_for_active_token(&token_id) {
                        filled_oc = Some(ui_oc);
                        let realized = self.position_mut(ui_oc).apply_fill(side, qty, price);
                        self.realized_pnl += realized;
                        match side {
                            Side::Buy => *self.fak_net_mut(ui_oc) += qty,
                            Side::Sell => {
                                let v = self.fak_net_mut(ui_oc);
                                *v = (*v - qty).max(0.0);
                            }
                        }
                        if qty > 1e-12 {
                            let fill = Fill {
                                ts: Utc::now(),
                                side,
                                outcome: ui_oc,
                                qty,
                                price,
                                realized,
                                clob_trade_id: None,
                            };
                            self.maybe_log_round_fill(&fill);
                            self.fills.push_back(fill);
                        }
                    } else {
                        self.apply_background_trailing_fill(&token_id, side, qty);
                    }
                    trim_fills_to_cap(&mut self.fills, 64);
                    if let Some(ref oid) = clob_order_id {
                        self.user_trade_sync
                            .after_order_ack_applied(oid, qty, price)
                            .await;
                    }
                    if let Some(oc) = filled_oc {
                        self.reconcile_position_shares_with_fill_deque(oc);
                    }
                    if side == Side::Buy && self.buy_trail_bps > 0 && qty > 1e-12 {
                        if let Some(oc) = filled_oc {
                            self.refresh_buy_trailing_from_position(oc, &token_id);
                        }
                    }
                    let skip_bump_and_trail_promote = matches!(side, Side::Buy)
                        && self.buy_trail_bps > 0
                        && self.outcome_for_active_token(&token_id).is_some();
                    if !skip_bump_and_trail_promote {
                        self.bump_trailing_tracked_shares(&token_id, side, qty);
                    }
                    if side == Side::Sell {
                        let rearmed = self.trailing_sell_in_flight.contains(&token_id)
                            && self.rearm_trailing_after_partial_sell(&token_id, qty);
                        if !rearmed {
                            self.clear_trailing_on_sell_token(&token_id);
                        }
                    } else if !skip_bump_and_trail_promote {
                        self.try_promote_pending_trail_any(token_id.as_str());
                    }
                }
                self.status_line = format!(
                    "{} {qty:.2} {} @ {price:.2} ✓",
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
                    self.merge_pending_trailing_buy_arm(
                        outcome,
                        entry_price,
                        plan_sell_shares,
                        token_id,
                        trail_bps,
                        activation_bps,
                        market,
                    );
                }
            }
            AppEvent::OrderErr(e) => self.status_line = format!("✗ {e}"),
            AppEvent::AutoTradingBuyDone {
                token_id,
                order_id,
                filled_qty,
                error,
            } => {
                let token_id = canonical_clob_token_id(&token_id).into_owned();
                if let Some(qty) = filled_qty {
                    self.autotrading_apply_fill(&token_id, Side::Buy, qty);
                    self.status_line =
                        format!("autotrading: BUY filled {qty:.2} shares; position tracked");
                } else if let Some(e) = error {
                    self.autotrading_clear_in_flight(&token_id);
                    self.status_line = format!("autotrading: stopped — {e}");
                } else {
                    if let Some(order_id) = order_id.as_deref() {
                        self.autotrading_track_resting_buy_order(order_id, &token_id);
                    }
                    self.status_line =
                        "autotrading: GTD BUY accepted (resting or no fill in response); \
                                        capacity reserved until maker fill or close"
                            .into();
                }
            }
            AppEvent::AutoTradingBuyOrderClosed { order_id } => {
                if self.autotrading_clear_resting_buy_order(&order_id) {
                    self.status_line =
                        "autotrading: GTD BUY closed unfilled; reserved capacity released".into();
                }
            }
            AppEvent::RedeemDone { auto, result } => {
                self.redeem_clear_in_flight();
                match result {
                    Ok(summary) => {
                        self.status_line = if auto {
                            format!("auto CTF redeem: {summary}")
                        } else {
                            format!("CTF redeem: {summary}")
                        };
                    }
                    Err(e) => {
                        self.status_line = if auto {
                            format!("✗ auto redeem: {e}")
                        } else {
                            format!("✗ redeem: {e}")
                        };
                    }
                }
            }
            AppEvent::OrderErrModal(e) => {
                self.status_line = format!("✗ {e}");
                self.order_error_toast = Some(OrderErrorToast {
                    message: e,
                    until: Instant::now() + ORDER_ERROR_TOAST_TTL,
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
                self.cached_pair_label = format!("{}/USD", p.asset.label);
                self.cached_sentiment = SentimentDir::Unknown;
                self.cached_countdown_secs = None;
                self.buy_trail_bps = 0;
                self.buy_trail_activation_bps = 0;
                self.pending_ws_take_profit = None;
                self.book_watch_tokens_dirty = true;
            }
            AppEvent::RunTakeProfitAfterBuy { .. } => {
                // Dispatched from `main::apply_app_event` (needs `TradingClient`); no UI state change here.
            }
            AppEvent::MergeTakeProfitRestingSells { .. } => {
                // Dispatched from `main::apply_app_event` (user WS merge path).
            }
            AppEvent::StopLossSellDone {
                token_id,
                success,
                error,
            } => {
                if !success {
                    self.clear_stop_loss_sell_in_flight(&token_id);
                }
                if let Some(e) = error {
                    self.status_line = format!("✗ {e}");
                    self.order_error_toast = Some(OrderErrorToast {
                        message: e,
                        until: Instant::now() + ORDER_ERROR_TOAST_TTL,
                    });
                } else if success {
                    self.status_line = "stop-loss SELL submitted".into();
                }
            }
            AppEvent::TrailingExitDispatchDone {
                token_id,
                success,
                error,
            } => {
                let token_id = canonical_clob_token_id(&token_id).into_owned();
                // token_id is canonical; all keys/entries are canonical — direct equality suffices.
                self.trailing_sell_in_flight.retain(|k| *k != token_id);
                self.pending_trailing_sells
                    .retain(|p| p.token_id != token_id);
                self.book_watch_tokens_dirty = true;
                let tid = token_id.clone();
                if !success {
                    let had_sess = trailing_map_key_for_asset(&self.trailing, tid.as_str())
                        .and_then(|k| self.trailing.remove(&k));
                    let session_outcome = had_sess.as_ref().map(|s| s.outcome);
                    let mut re_armed = false;
                    if let Some(sess) = had_sess {
                        let pos_sh = self.trail_live_shares(&sess);
                        if pos_sh > CLOB_ORDER_REMAINING_DUST
                            && sess.trail_bps > 0
                            && sess.plan_sell_shares.is_finite()
                            && sess.plan_sell_shares > CLOB_ORDER_REMAINING_DUST
                        {
                            let entry = if self
                                .market
                                .as_ref()
                                .is_some_and(|m| m.condition_id == sess.market.condition_id)
                            {
                                let pos = self.position(sess.outcome);
                                if pos.avg_entry.is_finite() && pos.avg_entry > 0.0 {
                                    pos.avg_entry
                                } else {
                                    sess.stop.entry_price()
                                }
                            } else if let Some(oc) = self.outcome_for_active_token(&sess.token_id) {
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
                    if !re_armed {
                        self.trailing_sell_partial_fill_grace_until
                            .remove(tid.as_str());
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
                            until: Instant::now() + ORDER_ERROR_TOAST_TTL,
                        });
                    } else if re_armed {
                        let oc = session_outcome.unwrap_or(Outcome::Up);
                        self.status_line =
                            format!("trailing {} re-armed after exit failure", oc.as_str());
                    }
                } else if let Some(k) = trailing_map_key_for_asset(&self.trailing, tid.as_str()) {
                    let remove = match self.trailing.get(&k) {
                        Some(sess) => {
                            sess.stop.is_triggered()
                                || self.trail_live_shares(sess) <= CLOB_ORDER_REMAINING_DUST
                        }
                        None => true,
                    };
                    if remove {
                        self.trailing.remove(&k);
                        self.trailing_sell_partial_fill_grace_until
                            .remove(tid.as_str());
                    }
                }
            }
            AppEvent::Key(_) => {} // handled in main via `events::handle_key`
        }
        if self.book_watch_tokens_dirty {
            self.cached_book_watch_tokens = collect_book_watch_token_ids(self);
            // Prune watched_books only after the watch list changes.
            let tokens = &self.cached_book_watch_tokens;
            self.watched_books
                .retain(|k, _| tokens.binary_search(k).is_ok());
            self.book_watch_tokens_dirty = false;
        }
    }
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

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

fn fill_trade_id_key(clob_trade_id: Option<&str>) -> Option<String> {
    let id = clob_trade_id?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_ascii_lowercase())
    }
}

/// `GET /data/orders` snapshot for REST hydration: prefer **your** resting order's `side` and
/// `asset_id` over trade-row noise (same idea as user-channel [`try_parse_user_channel_trade`]).
#[derive(Debug, Clone, Default)]
pub struct HydrateOrderSnap {
    /// [`norm_order_id_key`] for each open order id.
    pub known_order_keys: HashSet<String>,
    pub side_by_norm_id: HashMap<String, Side>,
    pub asset_by_norm_id: HashMap<String, String>,
}

impl HydrateOrderSnap {
    pub fn from_open_orders(rows: &[ClobOpenOrder]) -> Self {
        let mut known_order_keys = HashSet::new();
        let mut side_by_norm_id = HashMap::new();
        let mut asset_by_norm_id = HashMap::new();
        for o in rows {
            let k = norm_order_id_key(&o.id);
            if k.is_empty() {
                continue;
            }
            known_order_keys.insert(k.clone());
            if let Some(s) = parse_clob_side_str(&o.side) {
                side_by_norm_id.insert(k.clone(), s);
            }
            asset_by_norm_id.insert(k, o.asset_id.clone());
        }
        Self {
            known_order_keys,
            side_by_norm_id,
            asset_by_norm_id,
        }
    }
}

/// Effective outcome-token id for one [`ClobMakerOrder`] leg (falls back to trade-level [`ClobTrade::asset_id`]).
fn eff_maker_leg_asset<'a>(t: &'a ClobTrade, mo: &'a ClobMakerOrder) -> &'a str {
    mo.asset_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(t.asset_id.as_str())
}

/// Picks the authenticated user's maker leg when `GET /data/trades` includes **multiple**
/// `maker_orders` (same aggregate match). Prefer a leg whose [`ClobMakerOrder::order_id`] is still in
/// [`HydrateOrderSnap::known_order_keys`] (from `GET /data/orders`), then a unique [`ClobMakerOrder::owner`]
/// matching the authenticated L2 `apiKey`, then the **unique** leg whose token differs from top-level
/// `asset_id` (observed when the row's `asset_id` is one outcome but the user's fill is on the sibling).
fn maker_leg_for_trade_row<'a>(
    t: &'a ClobTrade,
    up_token_id: &str,
    down_token_id: &str,
    order_snap: &HydrateOrderSnap,
    clob_api_key: Option<&str>,
) -> Option<&'a ClobMakerOrder> {
    let is_maker = matches!(
        t.trader_side
            .as_deref()
            .map(|s| s.trim().to_ascii_uppercase())
            .as_deref(),
        Some("MAKER") | Some("M")
    );
    if !is_maker {
        return None;
    }

    let in_market = |aid: &str| {
        clob_asset_ids_match(aid, up_token_id) || clob_asset_ids_match(aid, down_token_id)
    };

    let candidates: Vec<&ClobMakerOrder> = t
        .maker_orders
        .iter()
        .filter(|mo| in_market(eff_maker_leg_asset(t, mo)))
        .collect();

    if candidates.is_empty() {
        for mo in &t.maker_orders {
            if clob_asset_ids_match(eff_maker_leg_asset(t, mo), t.asset_id.as_str()) {
                return Some(mo);
            }
        }
        if t.maker_orders.len() == 1 {
            return t.maker_orders.first();
        }
        return None;
    }

    let ledger_hits: Vec<&ClobMakerOrder> = candidates
        .iter()
        .copied()
        .filter(|mo| {
            let nk = norm_order_id_key(&mo.order_id);
            !nk.is_empty() && order_snap.known_order_keys.contains(&nk)
        })
        .collect();
    if ledger_hits.len() == 1 {
        return Some(ledger_hits[0]);
    }

    if let Some(key) = clob_api_key.map(str::trim).filter(|s| !s.is_empty()) {
        let want = norm_clob_owner(key);
        let owner_hits: Vec<&ClobMakerOrder> = candidates
            .iter()
            .copied()
            .filter(|mo| {
                mo.owner
                    .as_deref()
                    .map(norm_clob_owner)
                    .as_ref()
                    .map(|w| w == &want)
                    .unwrap_or(false)
            })
            .collect();
        if owner_hits.len() == 1 {
            return Some(owner_hits[0]);
        }
    }

    if candidates.len() == 1 {
        return Some(candidates[0]);
    }

    let top = t.asset_id.as_str();
    let off_top: Vec<&ClobMakerOrder> = candidates
        .iter()
        .copied()
        .filter(|mo| !clob_asset_ids_match(eff_maker_leg_asset(t, mo), top))
        .collect();
    if off_top.len() == 1 {
        return Some(off_top[0]);
    }

    let on_top: Vec<&ClobMakerOrder> = candidates
        .iter()
        .copied()
        .filter(|mo| clob_asset_ids_match(eff_maker_leg_asset(t, mo), top))
        .collect();
    if on_top.len() == 1 {
        return Some(on_top[0]);
    }

    for mo in candidates.iter().copied() {
        if clob_asset_ids_match(eff_maker_leg_asset(t, mo), top) {
            return Some(mo);
        }
    }
    candidates.first().copied()
}

/// Taker (or role unknown) row, not the maker leg.
fn trade_row_is_not_maker(t: &ClobTrade) -> bool {
    !matches!(
        t.trader_side
            .as_deref()
            .map(|s| s.trim().to_ascii_uppercase())
            .as_deref(),
        Some("MAKER") | Some("M")
    )
}

/// Apply one REST-replayed fill. Taker **BUY** with both `makingAmount` and `takingAmount` uses
/// reported USDC for cost basis so `avg_entry` / uPnL match the exchange; otherwise [`Position::apply_fill`].
/// Returns realized PnL and the `price` to store on the [`Fill`] row (USDC per share for the buy-usdc path).
fn hydrate_apply_position_fill(
    pos: &mut Position,
    t: &ClobTrade,
    side: Side,
    qty: f64,
    price: f64,
) -> (f64, f64) {
    if side == Side::Buy && trade_row_is_not_maker(t) {
        let mk = t
            .making_amount
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let tk = t
            .taking_amount
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let (Some(mk_s), Some(tk_s)) = (mk, tk) {
            if let (Ok(usdc), Ok(share_amt)) = (mk_s.parse::<f64>(), tk_s.parse::<f64>()) {
                if usdc.is_finite() && usdc > 0.0 && share_amt.is_finite() && share_amt > 0.0 {
                    let tol = f64::max(1e-6, 1e-4 * qty.max(share_amt).max(1.0));
                    if (share_amt - qty).abs() <= tol {
                        let r = pos.apply_buy_with_total_usdc(qty, usdc);
                        let fill_px = (usdc / qty).max(1e-12);
                        return (r, fill_px);
                    }
                }
            }
        }
    }
    (pos.apply_fill(side, qty, price), price)
}

/// `GET /data/trades` sometimes copies top-level trade [`ClobTrade::side`] (taker) into
/// `maker_orders[].side` for the user's leg. On one outcome token the resting maker and the
/// aggressor must be on opposite sides — if they parse equal, trust inversion of the trade `side`
/// ([L2 MakerOrder.side](https://docs.polymarket.com/developers/CLOB/clients/methods-l2)).
fn rest_maker_side_sanitize_taker_echo(taker_side: Side, resolved: Side) -> Side {
    if resolved == taker_side {
        match taker_side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    } else {
        resolved
    }
}

/// Side, shares, and price **for the authenticated user** on this `GET /data/trades` row.
///
/// When you are **MAKER**, share count is [`ClobMakerOrder::matched_amount`] on the **resolved** maker
/// leg only. **Side** and outcome **token** prefer [`HydrateOrderSnap`] from `GET /data/orders` when
/// the leg's `order_id` matches an open order (same as user-channel ledger). Otherwise use the leg +
/// [`rest_maker_side_sanitize_taker_echo`]. When you are **TAKER**, prefer `takingAmount` /
/// `makingAmount` (same as [`PostOrderResponse`]) for share count; `size` alone can disagree.
/// ([L2 getTrades](https://docs.polymarket.com/developers/CLOB/clients/methods-l2)).
/// When `Some`, use that string (leg token) for [`Outcome`] mapping instead of top-level [`ClobTrade::asset_id`].
fn rest_trade_user_fill(
    t: &ClobTrade,
    up_token_id: &str,
    down_token_id: &str,
    order_snap: &HydrateOrderSnap,
    clob_api_key: Option<&str>,
) -> Option<(Side, f64, f64, Option<String>)> {
    let taker_side_row = parse_clob_side(&t.side)?;
    let Ok(price_top) = t.price.parse::<f64>() else {
        return None;
    };
    if !price_top.is_finite() || price_top <= 0.0 {
        return None;
    }

    let role = t
        .trader_side
        .as_deref()
        .map(|s| s.trim().to_ascii_uppercase());
    let is_maker = matches!(role.as_deref(), Some("MAKER") | Some("M"));

    if !is_maker {
        let nk = t
            .taker_order_id
            .as_deref()
            .map(norm_order_id_key)
            .filter(|k| !k.is_empty());
        let mut taker_side = taker_side_row;
        if let Some(ref k) = nk {
            if let Some(&s) = order_snap.side_by_norm_id.get(k) {
                taker_side = s;
            }
        }
        let fill_asset = nk.and_then(|k| order_snap.asset_by_norm_id.get(&k).cloned());
        let qty = taker_trade_fill_shares(t, taker_side)?;
        return Some((taker_side, qty, price_top, fill_asset));
    }

    if let Some(leg) =
        maker_leg_for_trade_row(t, up_token_id, down_token_id, order_snap, clob_api_key)
    {
        let nk_leg = norm_order_id_key(&leg.order_id);
        let side_from_leg = leg
            .side
            .as_deref()
            .and_then(parse_clob_side)
            .unwrap_or_else(|| match taker_side_row {
                Side::Buy => Side::Sell,
                Side::Sell => Side::Buy,
            });
        let eff_a = eff_maker_leg_asset(t, leg);
        let side = if let Some(&s) = order_snap.side_by_norm_id.get(&nk_leg) {
            s
        } else if clob_asset_ids_match(eff_a, t.asset_id.as_str()) {
            rest_maker_side_sanitize_taker_echo(taker_side_row, side_from_leg)
        } else {
            side_from_leg
        };
        let qty = leg.matched_amount.trim().parse::<f64>().ok()?;
        if !qty.is_finite() || qty <= 0.0 {
            return None;
        }
        let px = match leg
            .price
            .as_ref()
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            Some(p) if p.is_finite() && p > 0.0 => p,
            _ => price_top,
        };
        let fill_asset = order_snap
            .asset_by_norm_id
            .get(&nk_leg)
            .cloned()
            .or_else(|| Some(eff_maker_leg_asset(t, leg).to_string()));
        return Some((side, qty, px, fill_asset));
    }

    // Legacy / sparse payloads: no maker leg — fall back to inverted taker + top `size` (best effort).
    let qty = t.size.parse::<f64>().ok()?;
    if !qty.is_finite() || qty <= 0.0 {
        return None;
    }
    let side = match taker_side_row {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    };
    Some((side, qty, price_top, None))
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
        let Some(side) = parse_clob_side(&o.side) else {
            continue;
        };
        if side != Side::Sell {
            continue;
        }
        if !clob_order_has_open_size(o) {
            continue;
        }
        let remaining = clob_order_remaining_size(o);
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
        let Some(side) = parse_clob_side(&o.side) else {
            continue;
        };
        let price = o.price.parse::<f64>().unwrap_or(f64::NAN);
        if !clob_order_has_open_size(&o) {
            continue;
        }
        let remaining = clob_order_remaining_size(&o);
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
            (Outcome::Up, Outcome::Up) | (Outcome::Down, Outcome::Down) => {
                std::cmp::Ordering::Equal
            }
            (Outcome::Up, Outcome::Down) => std::cmp::Ordering::Less,
            (Outcome::Down, Outcome::Up) => std::cmp::Ordering::Greater,
        };
        ord_o.then_with(|| {
            a.price
                .partial_cmp(&b.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
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
    let spendable = if balance.is_finite() {
        balance.max(0.0)
    } else {
        0.0
    };
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
    let data_avg = data_api
        .and_then(|(s, a)| (s.is_finite() && s > 1e-12 && a.is_finite() && a > 0.0).then_some(a));

    // Data API `size` / `avgPrice` agrees with our resolved `shares` (5% slack for REST vs wallet).
    let data_basis_for_shares = |s: f64| -> Option<f64> {
        if data_shares <= 1e-12 {
            return None;
        }
        let da = data_avg?;
        if (s - data_shares).abs() > f64::max(1e-6, 0.05 * s.max(data_shares)) {
            return None;
        }
        Some(da)
    };

    if !replay_ok && !bal_ok && !escrow_ok && data_shares < 1e-12 {
        return Position::default();
    }

    // Trade replay can lag on-chain / POST rounding (e.g. buy fill vs USDC-notional estimate).
    // Prefer the larger of replay and spendable+escrow so market SELL can flatten fully.
    let shares = if replay_ok {
        replay.shares.max(if inventory_fallback > 1e-12 {
            inventory_fallback
        } else {
            0.0
        })
    } else if inventory_fallback > 1e-12 {
        inventory_fallback
    } else if data_shares > 1e-12 {
        data_shares
    } else {
        0.0
    };

    let mut avg_entry = if replay_ok {
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
    } else if let Some(a) = data_basis_for_shares(shares) {
        a
    } else {
        0.0
    };

    // `/data/trades` missing or empty: wallet shows shares but CLOB VWAP is 0 → uPnL would read
    // as pure mark-to-zero-cost. Use Data API when its `size` matches inventory.
    if shares > 1e-9 && (!avg_entry.is_finite() || avg_entry <= 1e-12) {
        if let Some(a) = data_basis_for_shares(shares) {
            avg_entry = a;
        }
    }

    // Replay covers fewer shares than wallet (skipped rows, role filter, etc.): carrying
    // `replay.avg_entry` across the extra size understates cost and inflates uPnL after restart.
    if replay_ok && replay.avg_entry > 1e-12 {
        let gap = shares - replay.shares;
        let gap_significant =
            gap > f64::max(0.02, 0.005 * f64::max(shares, replay.shares.max(1.0)));
        if gap_significant {
            if let Some(a) = data_basis_for_shares(shares) {
                avg_entry = a;
            }
        }
    }

    Position { shares, avg_entry }
}

/// `true` if this trade row involves the active market's UP or DOWN token (top-level and/or maker legs).
fn trade_touches_market_tokens(t: &ClobTrade, up_token_id: &str, down_token_id: &str) -> bool {
    if clob_asset_ids_match(t.asset_id.as_str(), up_token_id)
        || clob_asset_ids_match(t.asset_id.as_str(), down_token_id)
    {
        return true;
    }
    let is_maker = matches!(
        t.trader_side
            .as_deref()
            .map(|s| s.trim().to_ascii_uppercase())
            .as_deref(),
        Some("MAKER") | Some("M")
    );
    if !is_maker {
        return false;
    }
    t.maker_orders.iter().any(|mo| {
        let a = eff_maker_leg_asset(t, mo);
        clob_asset_ids_match(a, up_token_id) || clob_asset_ids_match(a, down_token_id)
    })
}

/// Replay Polymarket `GET /data/trades` for the active market to recover VWAP entries and fills.
#[allow(clippy::too_many_arguments)]
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
    open_orders: &[ClobOpenOrder],
    clob_api_key: Option<&str>,
) -> (Position, Position, Vec<Fill>) {
    let order_snap = HydrateOrderSnap::from_open_orders(open_orders);
    let mut indexed: Vec<(DateTime<Utc>, &str, &ClobTrade)> = trades
        .iter()
        .filter(|t| {
            t.is_valid_fill()
                && t.trader_side.is_some()
                && trade_touches_market_tokens(t, up_token_id, down_token_id)
        })
        .map(|t| (parse_trade_timestamp(&t.match_time), t.id.as_str(), t))
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));

    let mut up = Position::default();
    let mut down = Position::default();
    let mut fills_chrono: Vec<Fill> = Vec::new();
    let mut seen_trade_ids: HashSet<String> = HashSet::new();

    for (_, _, t) in indexed {
        let Some((side, qty, price, fill_asset)) =
            rest_trade_user_fill(t, up_token_id, down_token_id, &order_snap, clob_api_key)
        else {
            debug!(id = %t.id, side = %t.side, "skip trade: bad side/qty/price for REST row");
            continue;
        };
        if !qty.is_finite() || qty <= 0.0 || !price.is_finite() {
            continue;
        }
        let aid_fill = fill_asset
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(t.asset_id.as_str());
        if !(clob_asset_ids_match(aid_fill, up_token_id)
            || clob_asset_ids_match(aid_fill, down_token_id))
        {
            debug!(id = %t.id, aid = %aid_fill, "skip trade: resolved fill asset not in active UP/DOWN pair");
            continue;
        }
        let outcome = if clob_asset_ids_match(aid_fill, up_token_id) {
            Outcome::Up
        } else {
            Outcome::Down
        };
        let trade_id = t.id.trim();
        if let Some(key) = fill_trade_id_key(Some(trade_id)) {
            if !seen_trade_ids.insert(key) {
                continue;
            }
        }
        let ts = parse_trade_timestamp(&t.match_time);
        let (realized, fill_price) = match outcome {
            Outcome::Up => hydrate_apply_position_fill(&mut up, t, side, qty, price),
            Outcome::Down => hydrate_apply_position_fill(&mut down, t, side, qty, price),
        };
        fills_chrono.push(Fill {
            ts,
            side,
            outcome,
            qty,
            price: fill_price,
            realized,
            clob_trade_id: if trade_id.is_empty() {
                None
            } else {
                Some(trade_id.to_string())
            },
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

/// VWAP price over **BUY** fills for `outcome` in the session deque (for avg_entry repair).
fn vwap_buy_price_from_fills_for_outcome(fills: &VecDeque<Fill>, outcome: Outcome) -> f64 {
    let mut q = 0.0_f64;
    let mut pc = 0.0_f64;
    for f in fills.iter() {
        if f.outcome != outcome || f.side != Side::Buy {
            continue;
        }
        if f.qty.is_finite() && f.qty > 0.0 && f.price.is_finite() && f.price > 0.0 {
            q += f.qty;
            pc += f.qty * f.price;
        }
    }
    if q > 1e-12 {
        pc / q
    } else {
        0.0
    }
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

/// Gross minimum edge vs `entry_px` for a trailing FAK sell floor (after sell slippage).
/// `min_profit_bps == 0` disables the check. Invalid prices return `true` so dispatch is not stuck.
#[inline]
pub fn trailing_exit_sell_meets_min_gross_profit_bps(
    sell_floor_px: f64,
    entry_px: f64,
    min_profit_bps: u32,
) -> bool {
    if min_profit_bps == 0 {
        return true;
    }
    if !sell_floor_px.is_finite() || !entry_px.is_finite() || entry_px <= 0.0 {
        return true;
    }
    let threshold = entry_px * (1.0 + min_profit_bps as f64 / 10_000.0);
    sell_floor_px + 1e-12 >= threshold
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

    use crate::feeds::clob_ws::{BookLevel, BookSnapshot, ClobTradePrint};
    use crate::feeds::user_trade_sync::UserTradeSync;
    use crate::fees::polymarket_crypto_taker_fee_usdc;
    use crate::gamma::ActiveMarket;
    use crate::strategy::ManualSignalLabel;
    use crate::trading::{ClobMakerOrder, ClobOpenOrder, ClobTrade};
    use chrono::Utc;

    fn test_state() -> AppState {
        AppState::new(5.0, 0.50, Arc::new(UserTradeSync::new()))
    }

    fn test_market(up: &str, down: &str, cond: &str) -> ActiveMarket {
        ActiveMarket {
            condition_id: cond.into(),
            question: "test".into(),
            slug: "test".into(),
            up_token_id: up.into(),
            down_token_id: down.into(),
            tick_size: "0.01".into(),
            neg_risk: false,
            price_to_beat: None,
            opens_at: Utc::now(),
            closes_at: Utc::now(),
            crypto_price_query_start_utc: String::new(),
            crypto_price_query_end_utc: String::new(),
        }
    }

    fn public_print(asset_id: &str, price: f64, size: f64, ts_ms: i64) -> ClobTradePrint {
        ClobTradePrint {
            asset_id: asset_id.to_string(),
            price,
            size,
            ts_ms,
        }
    }

    fn book(asset_id: &str, bid: f64, ask: f64, ask_size: f64) -> BookSnapshot {
        BookSnapshot {
            asset_id: asset_id.to_string(),
            bids: vec![BookLevel {
                price: bid,
                size: 50.0,
            }],
            asks: vec![BookLevel {
                price: ask,
                size: ask_size,
            }],
        }
    }

    fn fill(side: Side, outcome: Outcome, qty: f64, price: f64) -> Fill {
        Fill {
            ts: Utc::now(),
            side,
            outcome,
            qty,
            price,
            realized: 0.0,
            clob_trade_id: None,
        }
    }

    #[test]
    fn stop_loss_candidate_uses_highest_buy_fill_as_reference() {
        let mut s = test_state();
        let m = test_market("UP_SL", "DOWN_SL", "0xsl");
        s.market = Some(m);
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.40,
        };
        s.book_up = Some(Arc::new(book("UP_SL", 0.62, 0.64, 100.0)));
        s.fills.push_back(fill(Side::Buy, Outcome::Up, 5.0, 0.52));
        s.fills.push_back(fill(Side::Buy, Outcome::Up, 5.0, 0.70));

        let candidate = s.stop_loss_sell_candidate(1_000).expect("stop-loss candidate");

        assert_eq!(candidate.outcome, Outcome::Up);
        assert_eq!(candidate.token_id, "UP_SL");
        assert!((candidate.reference_price - 0.70).abs() < 1e-12);
        assert!((candidate.current_price - 0.62).abs() < 1e-12);
        assert!((candidate.limit_price - 0.61).abs() < 1e-12);
        assert!((candidate.shares - 10.0).abs() < 1e-12);
    }

    #[test]
    fn stop_loss_candidate_falls_back_to_avg_entry() {
        let mut s = test_state();
        let m = test_market("UP_SL_AVG", "DOWN_SL_AVG", "0xslavg");
        s.market = Some(m);
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.book_up = Some(Arc::new(book("UP_SL_AVG", 0.44, 0.46, 100.0)));

        let candidate = s.stop_loss_sell_candidate(1_000).expect("stop-loss candidate");

        assert_eq!(candidate.outcome, Outcome::Up);
        assert!((candidate.reference_price - 0.50).abs() < 1e-12);
        assert!((candidate.limit_price - 0.43).abs() < 1e-12);
    }

    #[test]
    fn stop_loss_candidate_ignores_opposite_outcome_buy_fills() {
        let mut s = test_state();
        let m = test_market("UP_SL_SIDE", "DOWN_SL_SIDE", "0xslside");
        s.market = Some(m);
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.book_up = Some(Arc::new(book("UP_SL_SIDE", 0.44, 0.46, 100.0)));
        s.fills.push_back(fill(Side::Buy, Outcome::Down, 5.0, 0.90));

        let candidate = s.stop_loss_sell_candidate(1_000).expect("stop-loss candidate");

        assert_eq!(candidate.outcome, Outcome::Up);
        assert!((candidate.reference_price - 0.50).abs() < 1e-12);
    }

    #[test]
    fn stop_loss_candidate_replaces_existing_resting_sell_but_skips_in_flight() {
        let mut s = test_state();
        let m = test_market("UP_SL_DUP", "DOWN_SL_DUP", "0xsldup");
        s.market = Some(m);
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.book_up = Some(Arc::new(book("UP_SL_DUP", 0.44, 0.46, 100.0)));
        s.open_orders.push(OpenOrderRow {
            side: Side::Sell,
            outcome: Outcome::Up,
            price: 0.43,
            remaining: 10.0,
        });

        assert!(
            s.stop_loss_sell_candidate(1_000).is_some(),
            "existing TP/resting sell should be replaced by stop-loss"
        );

        s.open_orders.clear();
        s.stop_loss_sell_in_flight.insert("UP_SL_DUP".into());

        assert!(s.stop_loss_sell_candidate(1_000).is_none());
    }

    #[tokio::test]
    async fn stop_loss_done_success_keeps_protection_marker() {
        let mut s = test_state();
        s.stop_loss_sell_in_flight.insert("UP_SL_DONE".into());

        s.apply(AppEvent::StopLossSellDone {
            token_id: "UP_SL_DONE".into(),
            success: true,
            error: None,
        })
        .await;

        assert!(s.stop_loss_sell_in_flight.contains("UP_SL_DONE"));
    }

    #[tokio::test]
    async fn stop_loss_done_failure_clears_in_flight_marker() {
        let mut s = test_state();
        s.stop_loss_sell_in_flight.insert("UP_SL_FAIL".into());

        s.apply(AppEvent::StopLossSellDone {
            token_id: "UP_SL_FAIL".into(),
            success: false,
            error: Some("failed".into()),
        })
        .await;

        assert!(s.stop_loss_sell_in_flight.is_empty());
    }

    #[test]
    fn app_state_exposes_manual_strategy_signal_from_live_ui_data() {
        let mut s = test_state();
        let now = Utc::now();
        let mut m = test_market("111", "222", "cond");
        m.price_to_beat = Some(100.0);
        m.opens_at = now - chrono::Duration::seconds(180);
        m.closes_at = now + chrono::Duration::seconds(120);
        s.market = Some(m);
        s.spot_price = Some(100.80);
        s.book_up = Some(Arc::new(book("111", 0.34, 0.35, 80.0)));
        s.book_down = Some(Arc::new(book("222", 0.65, 0.66, 80.0)));
        s.cached_countdown_secs = Some(120);
        s.cached_sentiment = SentimentDir::Down;
        s.cached_market_activity_notional = 500.0;

        assert_eq!(
            s.manual_signal_label(),
            crate::strategy::ManualSignalLabel::StrongUp
        );
    }

    #[test]
    fn manual_signal_labels_map_to_auto_buy_outcomes_only_when_strong() {
        assert_eq!(
            manual_signal_label_to_buy_outcome(ManualSignalLabel::StrongUp),
            Some(Outcome::Up)
        );
        assert_eq!(
            manual_signal_label_to_buy_outcome(ManualSignalLabel::StrongDown),
            Some(Outcome::Down)
        );
        assert_eq!(
            manual_signal_label_to_buy_outcome(ManualSignalLabel::Watch),
            None
        );
        assert_eq!(
            manual_signal_label_to_buy_outcome(ManualSignalLabel::NoTrade),
            None
        );
    }

    #[test]
    fn autotrading_ledger_counts_open_token_positions_and_respects_cap() {
        let mut s = test_state();
        s.autotrading_max_positions = 1;

        assert!(s.autotrading_can_buy("111"));

        s.autotrading_apply_fill("111", Side::Buy, 4.0);

        assert_eq!(s.autotrading_reserved_count(), 1);
        assert!(!s.autotrading_can_buy("111"));
        assert!(!s.autotrading_can_buy("222"));

        s.autotrading_apply_fill("111", Side::Sell, 4.0);

        assert_eq!(s.autotrading_reserved_count(), 0);
        assert!(s.autotrading_can_buy("222"));
    }

    #[test]
    fn autotrading_in_flight_blocks_duplicate_token_until_cleared() {
        let mut s = test_state();
        s.autotrading_max_positions = 2;

        assert!(s.autotrading_mark_in_flight("111".to_string()));
        assert!(!s.autotrading_mark_in_flight("111".to_string()));
        assert!(!s.autotrading_can_buy("111"));

        s.autotrading_clear_in_flight("111");

        assert!(s.autotrading_can_buy("111"));
    }

    #[test]
    fn autotrading_in_flight_reserves_position_capacity_across_tokens() {
        let mut s = test_state();
        s.autotrading_max_positions = 1;

        assert!(s.autotrading_mark_in_flight("111".to_string()));

        assert!(!s.autotrading_can_buy("222"));
        assert!(!s.autotrading_mark_in_flight("222".to_string()));
    }

    #[tokio::test]
    async fn autotrading_resting_buy_keeps_capacity_reserved_until_fill() {
        let mut s = test_state();
        s.autotrading_max_positions = 1;

        assert!(s.autotrading_mark_in_flight("111".to_string()));
        s.apply(AppEvent::AutoTradingBuyDone {
            token_id: "111".into(),
            order_id: Some("0xorder1".into()),
            filled_qty: None,
            error: None,
        })
        .await;

        assert!(!s.autotrading_can_buy("222"));

        s.apply(AppEvent::UserChannelFill {
            clob_trade_id: "fill-1".into(),
            order_leg_id: "order-1".into(),
            side: Side::Buy,
            outcome: Outcome::Up,
            token_id: "111".into(),
            qty: 5.0,
            price: 0.5,
            ts: Utc::now(),
            from_maker_leg: true,
        })
        .await;

        assert!(!s.autotrading_can_buy("222"));
    }

    #[tokio::test]
    async fn autotrading_cancelled_resting_buy_releases_reserved_capacity() {
        let mut s = test_state();
        s.autotrading_max_positions = 1;

        assert!(s.autotrading_mark_in_flight("111".to_string()));
        s.apply(AppEvent::AutoTradingBuyDone {
            token_id: "111".into(),
            order_id: Some("0xabc".into()),
            filled_qty: None,
            error: None,
        })
        .await;

        assert!(!s.autotrading_can_buy("222"));

        s.apply(AppEvent::AutoTradingBuyOrderClosed {
            order_id: "abc".into(),
        })
        .await;

        assert!(s.autotrading_can_buy("222"));
    }

    #[test]
    fn redeem_gate_blocks_overlap_and_respects_cooldown() {
        let mut s = test_state();
        let t0 = Instant::now();
        let cooldown = Duration::from_secs(30);

        assert!(s.redeem_mark_in_flight(t0, cooldown));
        assert!(!s.redeem_mark_in_flight(t0 + Duration::from_secs(1), cooldown));

        s.redeem_clear_in_flight();

        assert!(!s.redeem_mark_in_flight(t0 + Duration::from_secs(29), cooldown));
        assert!(s.redeem_mark_in_flight(t0 + cooldown, cooldown));
    }

    fn trade(
        id: &str,
        asset_id: &str,
        side: &str,
        size: &str,
        price: &str,
        match_time: &str,
    ) -> ClobTrade {
        trade_with_status(id, asset_id, side, size, price, match_time, Some("MINED"))
    }

    #[tokio::test]
    async fn public_trade_batches_update_activity_once_per_event() {
        let mut s = test_state();
        let m = test_market("111", "222", "cond");
        s.apply(AppEvent::MarketRoll {
            market: m,
            buy_trail_bps: 0,
            buy_trail_activation_bps: 0,
        })
        .await;

        s.apply(AppEvent::ClobPublicTrades(vec![
            public_print("111", 0.50, 10.0, Utc::now().timestamp_millis()),
            public_print("222", 0.25, 20.0, Utc::now().timestamp_millis()),
            public_print("333", 0.99, 50.0, Utc::now().timestamp_millis()),
        ]))
        .await;

        assert_eq!(s.cached_market_activity_notional, 10.0);
    }

    #[tokio::test]
    async fn price_event_does_not_rebuild_book_watch_tokens() {
        let mut s = test_state();
        s.cached_book_watch_tokens = vec!["sentinel".into()];

        s.apply(AppEvent::Price(PriceTick {
            price: 100.0,
            timestamp_ms: Utc::now().timestamp_millis() as u64,
        }))
        .await;

        assert_eq!(s.cached_book_watch_tokens, vec!["sentinel".to_string()]);
    }

    fn trade_with_status(
        id: &str,
        asset_id: &str,
        side: &str,
        size: &str,
        price: &str,
        match_time: &str,
        status: Option<&str>,
    ) -> ClobTrade {
        ClobTrade {
            id: id.to_string(),
            asset_id: asset_id.to_string(),
            side: side.to_string(),
            size: size.to_string(),
            price: price.to_string(),
            match_time: match_time.to_string(),
            status: status.map(|s| s.to_string()),
            taker_order_id: None,
            maker_orders: vec![],
            // `hydrate_positions_from_trades` only replays fills with a known role (matches CLOB user rows).
            trader_side: Some("TAKER".into()),
            making_amount: None,
            taking_amount: None,
        }
    }

    /// REST replay: `maker_orders[].side` + **`matched_amount`** (not top-level `size`, which is taker).
    #[test]
    fn hydrate_maker_prefers_maker_order_side() {
        let up = "111";
        let t = ClobTrade {
            id: "1".into(),
            asset_id: up.to_string(),
            side: "SELL".into(),
            size: "9999".into(),
            price: "0.5".into(),
            match_time: "0".into(),
            status: Some("MINED".into()),
            taker_order_id: Some("0xt".into()),
            maker_orders: vec![ClobMakerOrder {
                order_id: "o1".into(),
                matched_amount: "10".into(),
                asset_id: Some(up.to_string()),
                side: Some("BUY".into()),
                price: None,
                owner: None,
            }],
            trader_side: Some("MAKER".into()),
            making_amount: None,
            taking_amount: None,
        };
        let (pu, _, fills) = hydrate_positions_from_trades(
            &[t],
            up,
            "222",
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert!((pu.shares - 10.0).abs() < 1e-6, "shares={}", pu.shares);
        assert!(
            fills
                .iter()
                .all(|f| f.side == Side::Buy && (f.qty - 10.0).abs() < 1e-6),
            "fills={:?}",
            fills
        );
    }

    /// Regression: REST can echo trade-level `side` onto the maker leg; restart Fills must stay BUY.
    #[test]
    fn hydrate_maker_when_leg_side_echoes_taker_uses_opposite() {
        let up = "111";
        let t = ClobTrade {
            id: "1".into(),
            asset_id: up.to_string(),
            side: "SELL".into(),
            size: "100".into(),
            price: "0.55".into(),
            match_time: "0".into(),
            status: Some("MINED".into()),
            taker_order_id: Some("0xt".into()),
            maker_orders: vec![ClobMakerOrder {
                order_id: "0xmine".into(),
                matched_amount: "10".into(),
                asset_id: Some(up.to_string()),
                side: Some("SELL".into()),
                price: Some("0.55".into()),
                owner: None,
            }],
            trader_side: Some("MAKER".into()),
            making_amount: None,
            taking_amount: None,
        };
        let (_pu, _, fills) = hydrate_positions_from_trades(
            &[t],
            up,
            "222",
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].side, Side::Buy, "fills={:?}", fills);
    }

    /// Regression (runtime logs): several `maker_orders` on one match; top `asset_id` is UP but the
    /// user's maker leg is on DOWN — must not take the first UP leg.
    #[test]
    fn hydrate_maker_multi_leg_picks_user_leg_off_top_asset_when_unique() {
        let up = "111";
        let down = "222";
        let t = ClobTrade {
            id: "m1".into(),
            asset_id: up.to_string(),
            side: "BUY".into(),
            size: "100".into(),
            price: "0.55".into(),
            match_time: "0".into(),
            status: Some("MINED".into()),
            taker_order_id: Some("0xtaker".into()),
            maker_orders: vec![
                ClobMakerOrder {
                    order_id: "0xother_up".into(),
                    matched_amount: "5".into(),
                    asset_id: Some(up.to_string()),
                    side: Some("SELL".into()),
                    price: Some("0.55".into()),
                    owner: None,
                },
                ClobMakerOrder {
                    order_id: "0xuser_dn".into(),
                    matched_amount: "5".into(),
                    asset_id: Some(down.to_string()),
                    side: Some("BUY".into()),
                    price: Some("0.45".into()),
                    owner: None,
                },
            ],
            trader_side: Some("MAKER".into()),
            making_amount: None,
            taking_amount: None,
        };
        let (pu, pd, fills) = hydrate_positions_from_trades(
            &[t],
            up,
            down,
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert_eq!(fills.len(), 1, "fills={:?}", fills);
        let f = &fills[0];
        assert_eq!(f.outcome, Outcome::Down, "fills={:?}", fills);
        assert_eq!(f.side, Side::Buy);
        assert!((f.qty - 5.0).abs() < 1e-9);
        assert!((pd.shares - 5.0).abs() < 1e-9, "pd={:?}", pd);
        assert!(pu.shares.abs() < 1e-9, "pu={:?}", pu);
    }

    /// `GET /data/orders` row for your maker leg: **side** and **asset** beat bad trade JSON.
    #[test]
    fn hydrate_maker_prefers_open_order_side_and_asset_for_multi_leg() {
        let up = "111";
        let down = "222";
        let t = ClobTrade {
            id: "m2".into(),
            asset_id: up.to_string(),
            side: "BUY".into(),
            size: "100".into(),
            price: "0.55".into(),
            match_time: "0".into(),
            status: Some("MINED".into()),
            taker_order_id: Some("0xtaker".into()),
            maker_orders: vec![
                ClobMakerOrder {
                    order_id: "0xother_up".into(),
                    matched_amount: "5".into(),
                    asset_id: Some(up.to_string()),
                    side: Some("SELL".into()),
                    price: Some("0.55".into()),
                    owner: None,
                },
                ClobMakerOrder {
                    order_id: "0xuser_dn".into(),
                    matched_amount: "5".into(),
                    asset_id: Some(down.to_string()),
                    side: Some("BUY".into()),
                    price: Some("0.45".into()),
                    owner: None,
                },
            ],
            trader_side: Some("MAKER".into()),
            making_amount: None,
            taking_amount: None,
        };
        let open = vec![ClobOpenOrder {
            id: "0xuser_dn".into(),
            asset_id: down.to_string(),
            side: "SELL".into(),
            price: "0.45".into(),
            original_size: "10".into(),
            size_matched: "5".into(),
        }];
        let (_pu, _pd, fills) = hydrate_positions_from_trades(
            &[t],
            up,
            down,
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &open,
            None,
        );
        assert_eq!(fills.len(), 1, "fills={:?}", fills);
        let f = &fills[0];
        assert_eq!(f.outcome, Outcome::Down, "fills={:?}", fills);
        assert_eq!(f.side, Side::Sell, "fills={:?}", fills);
        assert!((f.qty - 5.0).abs() < 1e-9);
    }

    /// When no open-order match, `maker_orders[].owner` == L2 `apiKey` picks your leg (same as user WS).
    #[test]
    fn hydrate_maker_owner_field_disambiguates_legs() {
        let api = "my-l2-api-key-uuid";
        let up = "111";
        let down = "222";
        let t = ClobTrade {
            id: "own1".into(),
            asset_id: up.to_string(),
            side: "BUY".into(),
            size: "10".into(),
            price: "0.5".into(),
            match_time: "0".into(),
            status: Some("MINED".into()),
            taker_order_id: Some("0xt".into()),
            maker_orders: vec![
                ClobMakerOrder {
                    order_id: "0xa".into(),
                    matched_amount: "3".into(),
                    asset_id: Some(up.to_string()),
                    side: Some("SELL".into()),
                    price: None,
                    owner: Some("not-me".into()),
                },
                ClobMakerOrder {
                    order_id: "0xb".into(),
                    matched_amount: "7".into(),
                    asset_id: Some(down.to_string()),
                    side: Some("BUY".into()),
                    price: None,
                    owner: Some(api.into()),
                },
            ],
            trader_side: Some("MAKER".into()),
            making_amount: None,
            taking_amount: None,
        };
        let (_pu, pd, fills) = hydrate_positions_from_trades(
            &[t],
            up,
            down,
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            Some(api),
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].outcome, Outcome::Down);
        assert_eq!(fills[0].side, Side::Buy);
        assert!((fills[0].qty - 7.0).abs() < 1e-9);
        assert!((pd.shares - 7.0).abs() < 1e-9);
    }

    /// Maker fill qty comes from the user's maker leg (`matched_amount`), not top-level `size`
    /// (which can reflect the other side or the full match).
    #[test]
    fn hydrate_maker_qty_from_leg_matched_amount_ignores_top_size() {
        let up = "111";
        let t = ClobTrade {
            id: "sz1".into(),
            asset_id: up.to_string(),
            side: "SELL".into(),
            size: "5".into(),
            price: "0.5".into(),
            match_time: "0".into(),
            status: Some("MINED".into()),
            taker_order_id: Some("0xt".into()),
            maker_orders: vec![ClobMakerOrder {
                order_id: "o1".into(),
                matched_amount: "50".into(),
                asset_id: Some(up.to_string()),
                side: Some("BUY".into()),
                price: None,
                owner: None,
            }],
            trader_side: Some("MAKER".into()),
            making_amount: None,
            taking_amount: None,
        };
        let (pu, _, fills) = hydrate_positions_from_trades(
            &[t],
            up,
            "222",
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert_eq!(fills.len(), 1);
        assert!((fills[0].qty - 50.0).abs() < 1e-9, "fills={:?}", fills);
        assert!((pu.shares - 50.0).abs() < 1e-9, "pu={:?}", pu);
    }

    #[test]
    fn hydrate_maker_uses_leg_price_when_present() {
        let up = "111";
        let t = ClobTrade {
            id: "1".into(),
            asset_id: up.to_string(),
            side: "BUY".into(),
            size: "100".into(),
            price: "0.50".into(),
            match_time: "0".into(),
            status: Some("MINED".into()),
            taker_order_id: None,
            maker_orders: vec![ClobMakerOrder {
                order_id: "o1".into(),
                matched_amount: "4".into(),
                asset_id: Some(up.to_string()),
                side: Some("SELL".into()),
                price: Some("0.48".into()),
                owner: None,
            }],
            trader_side: Some("MAKER".into()),
            making_amount: None,
            taking_amount: None,
        };
        let (_, _, fills) = hydrate_positions_from_trades(
            &[t],
            up,
            "222",
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        let f = &fills[0];
        assert!((f.qty - 4.0).abs() < 1e-9);
        assert!((f.price - 0.48).abs() < 1e-9);
    }

    /// REST: as taker on BUY, `size` can disagree with true share count; `takingAmount` matches postOrder.
    #[test]
    fn hydrate_taker_buy_prefers_taking_amount() {
        let up = "111";
        let t = ClobTrade {
            id: "1".into(),
            asset_id: up.to_string(),
            side: "BUY".into(),
            size: "2.5".into(),
            price: "0.5".into(),
            match_time: "0".into(),
            status: Some("MINED".into()),
            taker_order_id: None,
            maker_orders: vec![],
            trader_side: Some("TAKER".into()),
            making_amount: Some("5.40".into()),
            taking_amount: Some("10".into()),
        };
        let (pu, _, fills) = hydrate_positions_from_trades(
            &[t],
            up,
            "222",
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert!((pu.shares - 10.0).abs() < 1e-6, "shares={}", pu.shares);
        assert!(
            (pu.avg_entry - 0.54).abs() < 1e-6,
            "avg_entry={}",
            pu.avg_entry
        );
        assert!(
            fills
                .iter()
                .all(|f| (f.qty - 10.0).abs() < 1e-6 && (f.price - 0.54).abs() < 1e-6),
            "fills={:?}",
            fills
        );
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
        let (pu, pd, fills) = hydrate_positions_from_trades(
            &trades,
            up,
            down,
            20.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
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
    fn hydrate_skips_failed_status_trades() {
        let up = "111";
        let down = "222";
        let trades = vec![
            trade_with_status("ok", up, "BUY", "10", "0.5", "1000", Some("CONFIRMED")),
            trade_with_status("bad", up, "BUY", "99", "0.5", "2000", Some("FAILED")),
        ];
        let (pu, _, fills) = hydrate_positions_from_trades(
            &trades,
            up,
            down,
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert!((pu.shares - 10.0).abs() < 1e-6, "shares={}", pu.shares);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].clob_trade_id, Some("ok".into()));
    }

    #[test]
    fn hydrate_dedupes_mined_to_confirmed_status_update_by_trade_id() {
        let up = "111";
        let down = "222";
        let trades = vec![
            trade_with_status("same-fill", up, "BUY", "10", "0.5", "1000", Some("MINED")),
            trade_with_status(
                "same-fill",
                up,
                "BUY",
                "10",
                "0.5",
                "1000",
                Some("CONFIRMED"),
            ),
        ];

        let (pu, _, fills) = hydrate_positions_from_trades(
            &trades,
            up,
            down,
            0.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );

        assert!((pu.shares - 10.0).abs() < 1e-6, "shares={}", pu.shares);
        assert_eq!(fills.len(), 1, "fills={:?}", fills);
        assert_eq!(fills[0].clob_trade_id, Some("same-fill".into()));
    }

    #[tokio::test]
    async fn user_channel_fill_dedupes_by_trade_id_inside_app_state() {
        let mut s = test_state();
        s.market = Some(test_market("111", "222", "cond"));

        let first_ts = Utc::now();
        s.apply(AppEvent::UserChannelFill {
            clob_trade_id: "same-fill".into(),
            order_leg_id: "0xorder1".into(),
            side: Side::Buy,
            outcome: Outcome::Up,
            token_id: "111".into(),
            qty: 10.0,
            price: 0.5,
            ts: first_ts,
            from_maker_leg: false,
        })
        .await;

        // The app state must remain the last de-dupe boundary even if the external sync guard
        // has been reset between a MINED delivery and the later CONFIRMED delivery.
        s.user_trade_sync.on_market_roll().await;
        s.apply(AppEvent::UserChannelFill {
            clob_trade_id: "same-fill".into(),
            order_leg_id: "0xorder1".into(),
            side: Side::Buy,
            outcome: Outcome::Up,
            token_id: "111".into(),
            qty: 10.0,
            price: 0.5,
            ts: first_ts + chrono::Duration::seconds(1),
            from_maker_leg: false,
        })
        .await;

        assert!((s.position_up.shares - 10.0).abs() < 1e-6);
        assert_eq!(s.fills.len(), 1, "fills={:?}", s.fills);
        assert_eq!(
            s.fills.front().and_then(|f| f.clob_trade_id.as_deref()),
            Some("same-fill")
        );
    }

    #[test]
    fn market_sell_sells_full_tracked_position_ignores_small_usdc_ticket() {
        let mut state = test_state();
        state.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "1".into(),
            bids: vec![BookLevel {
                price: 0.5,
                size: 1000.0,
            }],
            asks: vec![BookLevel {
                price: 0.51,
                size: 1000.0,
            }],
        }));
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
        state.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "1".into(),
            bids: vec![BookLevel {
                price: 0.5,
                size: 1000.0,
            }],
            asks: vec![BookLevel {
                price: 0.51,
                size: 1000.0,
            }],
        }));
        state.position_up = Position::default();
        let (shares, _, _) =
            resolve_market_order(&state, Outcome::Up, Side::Sell, 10.0, 0, 0).unwrap();
        assert!((shares - 20.0).abs() < 1e-9);
    }

    #[test]
    fn market_sell_uses_fill_net_when_larger_than_position() {
        let mut state = test_state();
        state.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "1".into(),
            bids: vec![BookLevel {
                price: 0.5,
                size: 1000.0,
            }],
            asks: vec![BookLevel {
                price: 0.51,
                size: 1000.0,
            }],
        }));
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
        state.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "1".into(),
            bids: vec![BookLevel {
                price: 0.5,
                size: 1000.0,
            }],
            asks: vec![BookLevel {
                price: 0.51,
                size: 1000.0,
            }],
        }));
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
        let (pu, _, fills) = hydrate_positions_from_trades(
            &trades,
            up,
            "222",
            6.0,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert!((pu.shares - 6.0).abs() < 1e-6);
        let buy_cost = 10.0f64.mul_add(0.5, polymarket_crypto_taker_fee_usdc(10.0, 0.5));
        let expect_avg = buy_cost / 10.0;
        assert!((pu.avg_entry - expect_avg).abs() < 1e-6);
        let sell_fill = fills.iter().find(|f| f.side == Side::Sell).unwrap();
        let expect_realized =
            4.0f64.mul_add(0.7, -polymarket_crypto_taker_fee_usdc(4.0, 0.7)) - 4.0 * expect_avg;
        assert!((sell_fill.realized - expect_realized).abs() < 1e-6);
    }

    /// Spendable conditional balance is low while shares are escrowed for a resting SELL; UI uses trades.
    #[test]
    fn hydrate_prefers_replay_when_balance_excludes_escrow() {
        let up = "111";
        let trades = vec![trade("a", up, "BUY", "10", "0.5", "1000")];
        let spendable = 4.0;
        let (pu, _, _) = hydrate_positions_from_trades(
            &trades,
            up,
            "222",
            spendable,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
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
        let (pu, _, _) = hydrate_positions_from_trades(
            &trades,
            up,
            "222",
            chain,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert!((pu.shares - 10.09).abs() < 1e-6);
    }

    #[test]
    fn hydrate_falls_back_to_balance_when_no_trades() {
        let up = "111";
        let trades: Vec<ClobTrade> = vec![];
        let (pu, pd, _) = hydrate_positions_from_trades(
            &trades,
            up,
            "222",
            3.5,
            0.0,
            0.0,
            0.0,
            None,
            None,
            &[],
            None,
        );
        assert!((pu.shares - 3.5).abs() < 1e-6);
        assert!(pd.shares.abs() < 1e-9);
    }

    /// After restart, `/data/trades` may be empty while Data API still has `avgPrice` — uPnL needs it.
    #[test]
    fn hydrate_balance_only_uses_data_api_avg_when_sizes_align() {
        let up = "111";
        let trades: Vec<ClobTrade> = vec![];
        let data = Some((3.5, 0.62));
        let (pu, _, _) = hydrate_positions_from_trades(
            &trades,
            up,
            "222",
            3.5,
            0.0,
            0.0,
            0.0,
            data,
            None,
            &[],
            None,
        );
        assert!((pu.shares - 3.5).abs() < 1e-6);
        assert!((pu.avg_entry - 0.62).abs() < 1e-9);
    }

    /// Trade replay under-counts vs wallet: prefer Data API VWAP for the full size so uPnL is not inflated.
    #[test]
    fn hydrate_partial_replay_prefers_data_api_when_inventory_gap_large() {
        let up = "111";
        let trades = vec![trade("a", up, "BUY", "5", "0.40", "1000")];
        let chain = 20.0_f64;
        let data = Some((20.0, 0.58));
        let (pu, _, _) = hydrate_positions_from_trades(
            &trades,
            up,
            "222",
            chain,
            0.0,
            0.0,
            0.0,
            data,
            None,
            &[],
            None,
        );
        assert!((pu.shares - 20.0).abs() < 1e-6);
        assert!((pu.avg_entry - 0.58).abs() < 1e-9);
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
        let (pu, pd, _) = hydrate_positions_from_trades(
            &trades,
            up,
            "222",
            0.0,
            0.0,
            escrow_u,
            escrow_d,
            None,
            None,
            &[],
            None,
        );
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
            &[],
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
            &[],
            None,
        );
        assert!((pu.shares - 8.0).abs() < 1e-6);
        assert!((pu.avg_entry - 0.44).abs() < 1e-6);
        assert!(pd.shares.abs() < 1e-9);
    }

    /// Trailing on token `OLD_UP` stays in state when the UI `market` is already a different pair.
    #[tokio::test]
    async fn trailing_session_survives_different_ui_market() {
        let mut s = test_state();
        let m_old = test_market("OLD_UP", "OLD_DOWN", "0xc0");
        let m_new = test_market("NEW_UP", "NEW_DOWN", "0xc1");
        s.market = Some(m_new.clone());
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.5,
            plan_sell_shares: 10.0,
            token_id: "OLD_UP".into(),
            trail_bps: 100,
            activation_bps: 0,
            market: m_old,
        })
        .await;
        let b_old = BookSnapshot {
            asset_id: "OLD_UP".into(),
            bids: vec![BookLevel {
                price: 0.49,
                size: 10.0,
            }],
            asks: vec![BookLevel {
                price: 0.51,
                size: 10.0,
            }],
        };
        s.apply(AppEvent::Book(b_old)).await;
        assert!(
            s.trailing.contains_key("OLD_UP") || s.pending_trail_arms.contains_key("OLD_UP"),
            "expected pending or armed trail on OLD_UP"
        );
        let b_new = BookSnapshot {
            asset_id: "NEW_UP".into(),
            bids: vec![BookLevel {
                price: 0.40,
                size: 5.0,
            }],
            asks: vec![BookLevel {
                price: 0.42,
                size: 5.0,
            }],
        };
        s.apply(AppEvent::Book(b_new)).await;
        assert!(
            s.trailing.contains_key("OLD_UP") || s.pending_trail_arms.contains_key("OLD_UP"),
            "OLD_UP trail should not be dropped when NEW_UP book updates"
        );
        assert!(s.best_bid_for_token("OLD_UP").is_some());
    }

    #[tokio::test]
    async fn market_roll_unregisters_previous_trailing_state() {
        let mut s = test_state();
        let m_old = test_market("OLD_UP_ROLL", "OLD_DOWN_ROLL", "0xc0roll");
        let m_new = test_market("NEW_UP_ROLL", "NEW_DOWN_ROLL", "0xc1roll");
        s.market = Some(m_old.clone());
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "OLD_UP_ROLL".into(),
            bids: vec![BookLevel {
                price: 0.55,
                size: 10.0,
            }],
            asks: vec![BookLevel {
                price: 0.57,
                size: 10.0,
            }],
        }));
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "OLD_UP_ROLL".into(),
            trail_bps: 100,
            activation_bps: 0,
            market: m_old.clone(),
        })
        .await;
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Down,
            entry_price: 0.50,
            plan_sell_shares: 5.0,
            token_id: "OLD_DOWN_ROLL".into(),
            trail_bps: 100,
            activation_bps: 500,
            market: m_old.clone(),
        })
        .await;
        s.pending_trailing_sells.push_back(TrailingExit {
            token_id: "OLD_UP_ROLL".into(),
            market: m_old,
            outcome: Outcome::Up,
            sell_shares: 3.0,
        });
        s.trailing_sell_in_flight.insert("OLD_UP_ROLL".into());
        s.trailing_sell_partial_fill_grace_until.insert(
            "OLD_UP_ROLL".into(),
            Instant::now() + Duration::from_secs(1),
        );
        assert!(!s.trailing.is_empty());
        assert!(!s.pending_trail_arms.is_empty());
        assert!(!s.pending_trailing_sells.is_empty());

        s.apply(AppEvent::MarketRoll {
            market: m_new,
            buy_trail_bps: 0,
            buy_trail_activation_bps: 0,
        })
        .await;

        assert!(s.trailing.is_empty());
        assert!(s.pending_trail_arms.is_empty());
        assert!(s.pending_trailing_sells.is_empty());
        assert!(s.trailing_sell_in_flight.is_empty());
        assert!(s.trailing_sell_partial_fill_grace_until.is_empty());
        assert_eq!(s.background_trail_count(), 0);
        assert_eq!(
            s.cached_book_watch_tokens,
            vec!["NEW_DOWN_ROLL".to_string(), "NEW_UP_ROLL".to_string()]
        );
    }

    /// Two FAK buys on the same outcome token before arming: pending plans must add, not overwrite.
    #[tokio::test]
    async fn trailing_pending_merges_two_buys_same_token() {
        let mut s = test_state();
        let m = test_market("UP_M", "DOWN_M", "0xmerge1");
        s.market = Some(m.clone());
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_M".into(),
            trail_bps: 100,
            activation_bps: 500,
            market: m.clone(),
        })
        .await;
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.60,
            plan_sell_shares: 5.0,
            token_id: "UP_M".into(),
            trail_bps: 100,
            activation_bps: 500,
            market: m.clone(),
        })
        .await;
        let p = s
            .pending_trail_arms
            .get("UP_M")
            .expect("merged pending trail");
        assert!(
            (p.plan_sell_shares - 15.0).abs() < 1e-9,
            "plan={}",
            p.plan_sell_shares
        );
        let want_entry = (10.0 * 0.50 + 5.0 * 0.60) / 15.0;
        assert!((p.entry_price - want_entry).abs() < 1e-9);
    }

    /// Second buy promotes while a trail is already armed: `plan_sell_shares` sums both legs.
    #[tokio::test]
    async fn trailing_install_merges_second_buy_while_armed() {
        let mut s = test_state();
        let m = test_market("U_MER", "D_MER", "0xmerge2");
        s.market = Some(m.clone());
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "U_MER".into(),
            bids: vec![BookLevel {
                price: 0.52,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.54,
                size: 100.0,
            }],
        }));
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "U_MER".into(),
            trail_bps: 200,
            activation_bps: 0,
            market: m.clone(),
        })
        .await;
        assert!(
            s.trailing.contains_key("U_MER"),
            "first arm should promote immediately (activation_bps=0)"
        );
        assert!((s.trailing["U_MER"].plan_sell_shares - 10.0).abs() < 1e-9);

        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.55,
            plan_sell_shares: 5.0,
            token_id: "U_MER".into(),
            trail_bps: 200,
            activation_bps: 0,
            market: m.clone(),
        })
        .await;
        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "U_MER".into(),
            bids: vec![BookLevel {
                price: 0.56,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.58,
                size: 100.0,
            }],
        }))
        .await;

        let sess = s
            .trailing
            .get("U_MER")
            .expect("session after second promote");
        assert!(
            (sess.plan_sell_shares - 15.0).abs() < 1e-9,
            "plan={}",
            sess.plan_sell_shares
        );
    }

    /// Partial BUY [`AppEvent::OrderAck`]s must refresh trailing from live position, not keep the
    /// initial `RequestTrailingArm` size until the next maker-only WSS merge.
    #[tokio::test]
    async fn trailing_plan_follows_partial_buy_order_acks() {
        let mut s = test_state();
        let m = test_market("UP_ACK", "DOWN_ACK", "0xackpart");
        s.market = Some(m.clone());
        s.buy_trail_bps = 100;
        s.buy_trail_activation_bps = 0;
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UP_ACK".into(),
            bids: vec![BookLevel {
                price: 0.55,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.57,
                size: 100.0,
            }],
        }));
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_ACK".into(),
            trail_bps: 100,
            activation_bps: 0,
            market: m.clone(),
        })
        .await;
        s.apply(AppEvent::OrderAck {
            side: Side::Buy,
            outcome: Outcome::Up,
            qty: 3.0,
            price: 0.50,
            clob_order_id: None,
            token_id: "UP_ACK".into(),
        })
        .await;
        let sh = s.position(Outcome::Up).shares;
        let sess = s.trailing.get("UP_ACK").expect("armed after first ack");
        assert!(
            (sess.plan_sell_shares - sh).abs() < 1e-6,
            "plan={}",
            sess.plan_sell_shares
        );
        s.apply(AppEvent::OrderAck {
            side: Side::Buy,
            outcome: Outcome::Up,
            qty: 4.0,
            price: 0.55,
            clob_order_id: None,
            token_id: "UP_ACK".into(),
        })
        .await;
        let sh = s.position(Outcome::Up).shares;
        let sess = s.trailing.get("UP_ACK").expect("armed after second ack");
        assert!(
            (sess.plan_sell_shares - sh).abs() < 1e-6,
            "plan={}",
            sess.plan_sell_shares
        );
        assert!((sh - 7.0).abs() < 1e-6);
    }

    /// A partial trailing FAK SELL must keep the unsold shares protected instead of treating any
    /// SELL ack as a full exit.
    #[tokio::test]
    async fn trailing_partial_sell_ack_rearms_remaining_position() {
        let mut s = test_state();
        let m = test_market("UP_PART_SELL", "DOWN_PART_SELL", "0xpartsell");
        s.market = Some(m.clone());
        s.buy_trail_bps = 100;
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UP_PART_SELL".into(),
            bids: vec![BookLevel {
                price: 0.55,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.57,
                size: 100.0,
            }],
        }));
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_PART_SELL".into(),
            trail_bps: 100,
            activation_bps: 0,
            market: m,
        })
        .await;
        assert!(s.trailing.contains_key("UP_PART_SELL"));
        s.trailing_sell_in_flight.insert("UP_PART_SELL".into());

        s.apply(AppEvent::OrderAck {
            side: Side::Sell,
            outcome: Outcome::Up,
            qty: 6.0,
            price: 0.54,
            clob_order_id: Some("partial-trailing-sell".into()),
            token_id: "UP_PART_SELL".into(),
        })
        .await;

        assert!((s.position(Outcome::Up).shares - 4.0).abs() < 1e-6);
        let sess = s
            .trailing
            .get("UP_PART_SELL")
            .expect("remaining shares stay protected");
        assert!(
            (sess.plan_sell_shares - 4.0).abs() < 1e-6,
            "plan={}",
            sess.plan_sell_shares
        );

        s.apply(AppEvent::TrailingExitDispatchDone {
            token_id: "UP_PART_SELL".into(),
            success: true,
            error: None,
        })
        .await;
        assert!(
            s.trailing.contains_key("UP_PART_SELL"),
            "dispatch success must not clear a re-armed residual trail"
        );
    }

    /// Delayed partial fills can arrive seconds apart. After the first partial fill re-arms the
    /// residual, a book tick before the later fill must not trigger a duplicate trailing SELL.
    #[tokio::test]
    async fn trailing_delayed_partial_fill_gap_does_not_duplicate_sell() {
        let mut s = test_state();
        let m = test_market("UP_DELAY_PART", "DOWN_DELAY_PART", "0xdelaypart");
        s.market = Some(m.clone());
        s.buy_trail_bps = 100;
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UP_DELAY_PART".into(),
            bids: vec![BookLevel {
                price: 0.55,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.57,
                size: 100.0,
            }],
        }));
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_DELAY_PART".into(),
            trail_bps: 100,
            activation_bps: 0,
            market: m,
        })
        .await;
        s.trailing_sell_in_flight.insert("UP_DELAY_PART".into());

        s.apply(AppEvent::OrderAck {
            side: Side::Sell,
            outcome: Outcome::Up,
            qty: 6.0,
            price: 0.54,
            clob_order_id: Some("delayed-partial-trailing-sell".into()),
            token_id: "UP_DELAY_PART".into(),
        })
        .await;
        s.apply(AppEvent::TrailingExitDispatchDone {
            token_id: "UP_DELAY_PART".into(),
            success: true,
            error: None,
        })
        .await;

        tokio::time::sleep(Duration::from_secs(2)).await;
        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_DELAY_PART".into(),
            bids: vec![BookLevel {
                price: 0.70,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.72,
                size: 100.0,
            }],
        }))
        .await;
        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_DELAY_PART".into(),
            bids: vec![BookLevel {
                price: 0.65,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.67,
                size: 100.0,
            }],
        }))
        .await;
        assert!(
            s.pending_trailing_sells.is_empty(),
            "partial-fill grace should block duplicate trailing sell during delayed fill gap"
        );

        tokio::time::sleep(Duration::from_secs(1)).await;
        s.apply(AppEvent::UserChannelFill {
            clob_trade_id: "delayed-partial-fill-2".into(),
            order_leg_id: "delayed-partial-trailing-sell".into(),
            side: Side::Sell,
            outcome: Outcome::Up,
            qty: 4.0,
            price: 0.54,
            token_id: "UP_DELAY_PART".into(),
            ts: Utc::now(),
            from_maker_leg: false,
        })
        .await;

        assert!(s.pending_trailing_sells.is_empty());
        assert!(
            !s.trailing.contains_key("UP_DELAY_PART"),
            "later fill should clear the re-armed residual trail"
        );
    }

    /// A near-full trailing SELL can leave CLOB/UI dust. That residual is below the exchange's
    /// usable order size, so trailing must unregister instead of re-arming a zero-amount SELL.
    #[tokio::test]
    async fn trailing_near_full_sell_ack_clears_dust_residual() {
        let mut s = test_state();
        let m = test_market("UP_DUST_SELL", "DOWN_DUST_SELL", "0xdustsell");
        s.market = Some(m.clone());
        s.buy_trail_bps = 100;
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UP_DUST_SELL".into(),
            bids: vec![BookLevel {
                price: 0.55,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.57,
                size: 100.0,
            }],
        }));
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_DUST_SELL".into(),
            trail_bps: 100,
            activation_bps: 0,
            market: m,
        })
        .await;
        s.trailing_sell_in_flight.insert("UP_DUST_SELL".into());

        s.apply(AppEvent::OrderAck {
            side: Side::Sell,
            outcome: Outcome::Up,
            qty: 10.0 - crate::take_profit::CLOB_ORDER_REMAINING_DUST / 2.0,
            price: 0.54,
            clob_order_id: Some("near-full-trailing-sell".into()),
            token_id: "UP_DUST_SELL".into(),
        })
        .await;

        assert!(
            s.position(Outcome::Up).shares < crate::take_profit::CLOB_ORDER_REMAINING_DUST,
            "test setup should leave only dust"
        );
        assert!(
            !s.trailing.contains_key("UP_DUST_SELL"),
            "dust residual should unregister trailing instead of re-arming"
        );
    }

    /// If only dust inventory remains when a trail trips, do not queue a FAK SELL that will round
    /// to zero maker/taker amounts.
    #[tokio::test]
    async fn trailing_trigger_with_dust_inventory_unregisters_without_queueing() {
        let mut s = test_state();
        let m = test_market("UP_DUST_TRIGGER", "DOWN_DUST_TRIGGER", "0xdusttrigger");
        s.market = Some(m.clone());
        s.position_up = Position {
            shares: crate::take_profit::CLOB_ORDER_REMAINING_DUST / 2.0,
            avg_entry: 0.50,
        };
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UP_DUST_TRIGGER".into(),
            bids: vec![BookLevel {
                price: 0.70,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.72,
                size: 100.0,
            }],
        }));
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_DUST_TRIGGER".into(),
            trail_bps: 500,
            activation_bps: 0,
            market: m,
        })
        .await;
        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_DUST_TRIGGER".into(),
            bids: vec![BookLevel {
                price: 0.70,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.72,
                size: 100.0,
            }],
        }))
        .await;

        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_DUST_TRIGGER".into(),
            bids: vec![BookLevel {
                price: 0.65,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.67,
                size: 100.0,
            }],
        }))
        .await;

        assert!(s.pending_trailing_sells.is_empty());
        assert!(
            !s.trailing.contains_key("UP_DUST_TRIGGER"),
            "dust-only position should unregister the triggered trail"
        );
    }

    /// If CLOB already shows a resting/open SELL for the outcome, those shares are already locked
    /// for sale; trailing must not submit another FAK SELL on the same position.
    #[tokio::test]
    async fn trailing_does_not_enqueue_when_open_sell_already_exists() {
        let mut s = test_state();
        let m = test_market("UP_OPEN_SELL", "DOWN_OPEN_SELL", "0xopensell");
        s.market = Some(m.clone());
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UP_OPEN_SELL".into(),
            bids: vec![BookLevel {
                price: 0.70,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.72,
                size: 100.0,
            }],
        }));
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_OPEN_SELL".into(),
            trail_bps: 500,
            activation_bps: 0,
            market: m,
        })
        .await;
        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_OPEN_SELL".into(),
            bids: vec![BookLevel {
                price: 0.70,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.72,
                size: 100.0,
            }],
        }))
        .await;
        s.open_orders.push(OpenOrderRow {
            side: Side::Sell,
            outcome: Outcome::Up,
            price: 0.66,
            remaining: 10.0,
        });

        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_OPEN_SELL".into(),
            bids: vec![BookLevel {
                price: 0.65,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.67,
                size: 100.0,
            }],
        }))
        .await;

        assert!(
            s.pending_trailing_sells.is_empty(),
            "open SELL should block duplicate trailing FAK; pending={:?}",
            s.pending_trailing_sells
        );
    }

    /// A stop-loss GTD SELL in flight/protecting the token already owns the exit path; trailing
    /// must not submit a second FAK SELL for the same shares.
    #[tokio::test]
    async fn trailing_does_not_enqueue_when_stop_loss_sell_in_flight() {
        let mut s = test_state();
        let m = test_market("UP_SL_TRAIL", "DOWN_SL_TRAIL", "0xsltrail");
        s.market = Some(m.clone());
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UP_SL_TRAIL".into(),
            bids: vec![BookLevel {
                price: 0.70,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.72,
                size: 100.0,
            }],
        }));
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_SL_TRAIL".into(),
            trail_bps: 500,
            activation_bps: 0,
            market: m,
        })
        .await;
        s.stop_loss_sell_in_flight.insert("UP_SL_TRAIL".into());

        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_SL_TRAIL".into(),
            bids: vec![BookLevel {
                price: 0.65,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.67,
                size: 100.0,
            }],
        }))
        .await;

        assert!(
            s.pending_trailing_sells.is_empty(),
            "stop-loss in-flight/protected marker should block duplicate trailing FAK"
        );
    }

    /// A trailing sell accepted by CLOB as live/open may not have fill amounts yet. Once dispatch
    /// reports that handoff as successful, the already-triggered trail must be removed so later
    /// book ticks do not try to sell the same locked position again.
    #[tokio::test]
    async fn trailing_dispatch_success_clears_triggered_session_without_fill_ack() {
        let mut s = test_state();
        let m = test_market("UP_LIVE_SELL", "DOWN_LIVE_SELL", "0xlivesell");
        s.market = Some(m.clone());
        s.position_up = Position {
            shares: 10.0,
            avg_entry: 0.50,
        };
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UP_LIVE_SELL".into(),
            bids: vec![BookLevel {
                price: 0.70,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.72,
                size: 100.0,
            }],
        }));
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: "UP_LIVE_SELL".into(),
            trail_bps: 500,
            activation_bps: 0,
            market: m,
        })
        .await;
        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_LIVE_SELL".into(),
            bids: vec![BookLevel {
                price: 0.70,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.72,
                size: 100.0,
            }],
        }))
        .await;
        s.apply(AppEvent::Book(BookSnapshot {
            asset_id: "UP_LIVE_SELL".into(),
            bids: vec![BookLevel {
                price: 0.65,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.67,
                size: 100.0,
            }],
        }))
        .await;
        assert!(s.trailing["UP_LIVE_SELL"].stop.is_triggered());
        s.trailing_sell_in_flight.insert("UP_LIVE_SELL".into());

        s.apply(AppEvent::TrailingExitDispatchDone {
            token_id: "UP_LIVE_SELL".into(),
            success: true,
            error: None,
        })
        .await;

        assert!(
            !s.trailing.contains_key("UP_LIVE_SELL"),
            "accepted open/live trailing sell should retire the triggered trail"
        );
    }

    /// CLOB book `asset_id` may be `0x…` while the armed session key is decimal — the same logical token.
    #[tokio::test]
    async fn trailing_book_tick_finds_session_when_ws_asset_id_hex_map_key_decimal() {
        let mut s = test_state();
        let tid_decimal = "10";
        let tid_hex = "0x0a";
        let m = test_market(tid_decimal, "222", "0xcondhex");
        s.market = Some(m.clone());
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: tid_hex.into(),
            bids: vec![BookLevel {
                price: 0.52,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.54,
                size: 100.0,
            }],
        }));
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 10.0,
            token_id: tid_decimal.into(),
            trail_bps: 200,
            activation_bps: 0,
            market: m,
        })
        .await;
        assert!(
            s.trailing.contains_key(tid_decimal),
            "session keyed by decimal id from arm request"
        );
        let b = BookSnapshot {
            asset_id: tid_hex.into(),
            bids: vec![BookLevel {
                price: 0.90,
                size: 10.0,
            }],
            asks: vec![BookLevel {
                price: 0.92,
                size: 10.0,
            }],
        };
        s.apply(AppEvent::Book(b)).await;
        let sess = s.trailing.get(tid_decimal).expect("session");
        let best = sess.stop.best_price().expect("best after ratchet");
        assert!(
            best > 0.51,
            "trailing must follow book updates when asset_id string form differs; best={best}"
        );
    }

    #[tokio::test]
    async fn positions_loaded_flat_status_does_not_show_zero_position_row() {
        let mut s = test_state();
        s.status_line = "New market: test".into();
        s.apply(AppEvent::PositionsLoaded {
            position_up: Position::default(),
            position_down: Position::default(),
            fills_bootstrap: vec![],
            refresh_status_line: true,
        })
        .await;

        assert_eq!(s.status_line, "No active CLOB positions on this market");
    }

    /// REST replay shows a flat book — drop armed / pending trails so we never FAK-sell air.
    /// In-memory position must not exceed REST for this load (see `PositionsLoaded` merge rule);
    /// when both are flat, `sync_trailing_inventory_with_positions` clears trails for this market.
    #[tokio::test]
    async fn positions_loaded_clears_trailing_when_flat() {
        let mut s = test_state();
        let m = test_market("UPL", "DPL", "0xrestflat");
        s.market = Some(m.clone());
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "UPL".into(),
            bids: vec![BookLevel {
                price: 0.52,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.54,
                size: 100.0,
            }],
        }));
        s.position_up = Position::default();
        s.apply(AppEvent::RequestTrailingArm {
            outcome: Outcome::Up,
            entry_price: 0.50,
            plan_sell_shares: 8.0,
            token_id: "UPL".into(),
            trail_bps: 200,
            activation_bps: 0,
            market: m.clone(),
        })
        .await;
        assert!(s.trailing.contains_key("UPL"));
        s.apply(AppEvent::PositionsLoaded {
            position_up: Position::default(),
            position_down: Position::default(),
            fills_bootstrap: vec![],
            refresh_status_line: false,
        })
        .await;
        assert!(!s.trailing.contains_key("UPL"));
        assert!(!s.pending_trail_arms.contains_key("UPL"));
    }

    /// With `buy_trail_bps` set (as after [`AppEvent::MarketRoll`]), REST hydration restores a
    /// pending trail for non-flat long inventory.
    #[tokio::test]
    async fn positions_loaded_rehydrates_pending_trail_when_trail_bps_set() {
        let mut s = test_state();
        let m = test_market("RH1", "RH2", "0xrehyd");
        s.market = Some(m.clone());
        s.buy_trail_bps = 150;
        // Keep pending: require bid >= entry × 2 before arm (bid 0.60 < 0.55 × 2).
        s.buy_trail_activation_bps = 10_000;
        s.book_up = Some(Arc::new(BookSnapshot {
            asset_id: "RH1".into(),
            bids: vec![BookLevel {
                price: 0.60,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.62,
                size: 100.0,
            }],
        }));
        s.apply(AppEvent::PositionsLoaded {
            position_up: Position {
                shares: 10.0,
                avg_entry: 0.55,
            },
            position_down: Position::default(),
            fills_bootstrap: vec![],
            refresh_status_line: false,
        })
        .await;
        assert!(
            s.pending_trail_arms.contains_key("RH1"),
            "expected pending trail for UP leg; pending={:?}",
            s.pending_trail_arms.keys().collect::<Vec<_>>()
        );
        assert!(!s.trailing.contains_key("RH1"));
    }

    #[test]
    fn trailing_exit_min_profit_bps_disabled_always_ok() {
        assert!(trailing_exit_sell_meets_min_gross_profit_bps(0.40, 0.50, 0));
    }

    #[test]
    fn trailing_exit_min_profit_bps_gross_edge_vs_entry() {
        let entry = 0.50;
        let bps = 100; // 1%
        let threshold = entry * 1.01;
        assert!(
            !trailing_exit_sell_meets_min_gross_profit_bps(threshold - 1e-6, entry, bps),
            "just below 1% gross should fail"
        );
        assert!(trailing_exit_sell_meets_min_gross_profit_bps(
            threshold, entry, bps
        ));
    }

    #[test]
    fn trailing_exit_min_profit_bps_skips_when_prices_invalid() {
        assert!(trailing_exit_sell_meets_min_gross_profit_bps(
            f64::NAN,
            0.5,
            50
        ));
        assert!(trailing_exit_sell_meets_min_gross_profit_bps(0.6, 0.0, 50));
    }
}
