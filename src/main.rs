//! Application entry point.
//!
//! Boots:
//!   1. Tracing → stderr (filtered by RUST_LOG).
//!   2. Trading client (derives L2 API creds on first use).
//!   3. Chainlink RTDS task (BTC price).
//!   4. Market discovery (Gamma poll) + auto-roll task.
//!   5. CLOB book subscription task — restarted on each market roll.
//!   6. Crossterm event task.
//!   7. 1-Hz ticker for countdown.
//!
//! All sources push into one `mpsc<AppEvent>`; the main loop drains that,
//! mutates `AppState`, and re-renders. Feed-heavy paths throttle redraws (see
//! `FEED_REDRAW_MIN`) so `Terminal::draw` does not run far above ~20 Hz — ratatui
//! issue #1338 / FAQ: `draw` dominates CPU if called on every RTDS/CLOB message.

mod app;
mod balances;
mod bridge_deposit;
mod config;
mod fees;
mod data_api;
mod redeem;
mod events;
mod feeds;
mod gamma;
mod market_profile;
mod gamma_series;

use market_profile::{MarketProfile, CRYPTO_ASSETS, Timeframe};
mod net;
mod trading;
mod trailing_stop;
mod ui;

use anyhow::{Context, Result};
use app::{
    clamp_prob, collect_book_watch_token_ids, escrow_sell_shares_from_clob_orders,
    hydrate_positions_from_trades,     AppEvent, AppState, Outcome, resolve_market_order, resolve_trailing_sell, TrailingExit,
    MIN_LIMIT_ORDER_SHARES,
};
use feeds::clob_user_ws::{UserWsBundle, UserWsMarket};
use fees::take_profit_limit_price_crypto_after_fees;
use config::Config;
use crossterm::{
    event::{
        DisableFocusChange, EnableFocusChange, Event as CtEvent, EventStream, KeyboardEnhancementFlags,
        KeyCode, KeyEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use events::Action;
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::{mpsc, watch};
use std::{
    collections::{HashMap, HashSet},
    io::stdout,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// Set when we `execute!(PushKeyboardEnhancementFlags)` so `FocusGained` can pop/push to resync
/// after the terminal loses track of modifier/lock state (see `events::normalize_terminal_key_event`).
static KEYBOARD_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

fn keyboard_protocol_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
}

/// Minimum gap between `Terminal::draw` calls when the batch had no keyboard events.
/// Caps redraw rate from Chainlink + CLOB (~20 Hz) without delaying key handling.
const FEED_REDRAW_MIN: Duration = Duration::from_millis(50);

/// After a market BUY, wait briefly for a **`trade`** on the CLOB user WebSocket so take-profit
/// size matches the pushed fill (often arrives before REST trade history).
const USER_WS_TP_FILL_WAIT: Duration = Duration::from_millis(80);

/// Retries (no added sleep here; each attempt awaits the full `place_order` future).
const TRAILING_EXIT_FAK_ATTEMPTS: u32 = 3;

/// Build CLOB user-channel subscription set (active UI + trailing tail markets).
fn build_user_ws_bundle(state: &AppState) -> UserWsBundle {
    let active = state
        .market
        .as_ref()
        .map(|m| UserWsMarket {
            condition_id:  m.condition_id.clone(),
            up_token_id:   m.up_token_id.clone(),
            down_token_id: m.down_token_id.clone(),
        })
        .unwrap_or_default();

    let mut seen: HashSet<String> = HashSet::new();
    if !active.condition_id.is_empty() {
        seen.insert(active.condition_id.clone());
    }
    let mut extras: Vec<UserWsMarket> = Vec::new();
    let mut push_extra = |mkt: &gamma::ActiveMarket| {
        if mkt.condition_id.is_empty() || seen.contains(&mkt.condition_id) {
            return;
        }
        seen.insert(mkt.condition_id.clone());
        extras.push(UserWsMarket {
            condition_id:  mkt.condition_id.clone(),
            up_token_id:   mkt.up_token_id.clone(),
            down_token_id: mkt.down_token_id.clone(),
        });
    };
    for sess in state.trailing.values() {
        if active.condition_id.is_empty() || sess.market.condition_id != active.condition_id {
            push_extra(&sess.market);
        }
    }
    for p in state.pending_trail_arms.values() {
        if active.condition_id.is_empty() || p.market.condition_id != active.condition_id {
            push_extra(&p.market);
        }
    }
    UserWsBundle { active, extras }
}

fn merge_ui_and_extra_book_tokens(ui_pair: &[String], extra: &[String]) -> Vec<String> {
    let mut s: HashSet<String> = HashSet::new();
    for t in ui_pair {
        s.insert(t.clone());
    }
    for t in extra {
        s.insert(t.clone());
    }
    let mut v: Vec<String> = s.into_iter().collect();
    v.sort();
    v
}

/// Applies one [`AppEvent`]. Returns `true` if the user requested [`Action::Quit`].
async fn apply_app_event(
    ev:     AppEvent,
    state:  &mut AppState,
    trading:&Arc<TradingClient>,
    tx:     &mpsc::Sender<AppEvent>,
    cfg:    &Config,
    user_open_ledger: &std::sync::Arc<feeds::clob_user_ws::UserOpenOrdersLedger>,
    rtds_sym_tx:     &watch::Sender<String>,
    market_tx:       &mpsc::Sender<gamma::ActiveMarket>,
    market_profile_tx: &watch::Sender<Arc<MarketProfile>>,
    market_profile_rx_slot: &mut Option<watch::Receiver<Arc<MarketProfile>>>,
    discovery_spawned: &mut bool,
    user_bundle_tx:  &watch::Sender<UserWsBundle>,
    book_token_tx:   &watch::Sender<Vec<String>>,
) -> bool {
    match ev {
        AppEvent::Key(k) => {
            let action = events::handle_key(state, k);
            if matches!(action, Action::Quit) {
                return true;
            }
            if matches!(action, Action::Claim) {
                info!(
                    key_code = ?k.code,
                    modifiers = ?k.modifiers,
                    "TUI: CTF redeem key pressed (dispatching redeem)"
                );
            }
            dispatch_action(action, state, trading, tx, cfg, user_open_ledger);
            false
        }
        e => {
            if let AppEvent::StartTrading(p) = &e {
                let _ = rtds_sym_tx.send(p.asset.rtds_symbol.to_string());
                let _ = market_profile_tx.send(p.clone());
                if !*discovery_spawned {
                    if let Some(rx) = market_profile_rx_slot.take() {
                        *discovery_spawned = true;
                        let txi = tx.clone();
                        let mtx = market_tx.clone();
                        tokio::spawn(async move {
                            feeds::market_discovery_gamma::spawn(txi, mtx, rx);
                        });
                    } else {
                        warn!("market discovery: profile receiver missing — cannot spawn Gamma poll");
                    }
                }
            }
            state.apply(e).await;
            try_dispatch_trailing_sell(
                state,
                trading,
                tx,
                user_open_ledger,
                cfg,
            );
            let _ = user_bundle_tx.send(build_user_ws_bundle(state));
            let _ = book_token_tx.send(collect_book_watch_token_ids(state));
            false
        }
    }
}

/// After a book tick trips the client-side trail, place a FAK SELL (with retries) as soon as the
/// CLOB resolves a price. Keeps `pending_trailing_sell` + `trailing` until an order is acked or
/// [`TRAILING_EXIT_FAK_ATTEMPTS`] fails.
fn try_dispatch_trailing_sell(
    state: &mut AppState,
    trading: &Arc<TradingClient>,
    tx: &mpsc::Sender<AppEvent>,
    user_open_ledger: &std::sync::Arc<feeds::clob_user_ws::UserOpenOrdersLedger>,
    cfg: &Config,
) {
    if state.trailing_sell_in_flight {
        return;
    }
    let ex = match state.pending_trailing_sell.clone() {
        Some(x) => x,
        None => return,
    };
    let res = resolve_trailing_sell(
        state,
        ex.token_id.as_str(),
        ex.sell_shares,
        cfg.market_sell_slippage_bps,
    );
    let Some((shares, price, otype)) = res else {
        // Empty book: keep `pending` + tripped `trailing` until the book can price a FAK SELL.
        return;
    };
    state.trailing_sell_in_flight = true;
    let exit = ex;
    let trading2 = trading.clone();
    let ledger2 = user_open_ledger.clone();
    let tx2 = tx.clone();
    info!(
        outcome = ?exit.outcome,
        sell_shares = shares,
        price,
        attempts = TRAILING_EXIT_FAK_ATTEMPTS,
        "trailing: FAK SELL (await place_order, up to {n} attempts)",
        n = TRAILING_EXIT_FAK_ATTEMPTS
    );
    tokio::spawn(async move {
        run_trailing_exit_fak_sell(trading2, ledger2, tx2, exit, shares, price, otype).await;
    });
}

async fn run_trailing_exit_fak_sell(
    trading: Arc<TradingClient>,
    user_open_ledger: std::sync::Arc<feeds::clob_user_ws::UserOpenOrdersLedger>,
    tx: mpsc::Sender<AppEvent>,
    exit: TrailingExit,
    shares: f64,
    price: f64,
    otype: OrderType,
) {
    let market_for_refresh = exit.market.clone();
    let token_id = exit.token_id.clone();
    let outcome = exit.outcome;
    for attempt in 1u32..=TRAILING_EXIT_FAK_ATTEMPTS {
        let args = OrderArgs {
            token_id:        token_id.clone(),
            side:            Side::Sell,
            price,
            size:            shares,
            neg_risk:        exit.market.neg_risk,
            tick_size:       exit.market.tick_size.clone(),
            buy_notional_usdc: None,
            expiration_unix_secs: 0,
            sell_skip_pre_post_settle: false,
        };
        let attempt_label = format!("{}/{}", attempt, TRAILING_EXIT_FAK_ATTEMPTS);
        let cli = trading.clone();
        match cli.place_order(args, otype).await {
            Ok(resp) => {
                // Same acceptance rule as `spawn_order` (FAK branch).
                let ok_ui = resp.success
                    || resp
                        .status
                        .as_ref()
                        .is_some_and(|s| {
                            s.eq_ignore_ascii_case("matched")
                                || s.eq_ignore_ascii_case("delayed")
                                || s.eq_ignore_ascii_case("live")
                                || s.eq_ignore_ascii_case("open")
                        });
                if !ok_ui {
                    let msg = resp
                        .error
                        .unwrap_or_else(|| format!("status={:?}", resp.status));
                    if attempt < TRAILING_EXIT_FAK_ATTEMPTS {
                        warn!(
                            %attempt_label,
                            outcome = ?outcome,
                            %msg,
                            "trailing exit: CLOB body not ok; retry"
                        );
                        continue;
                    }
                    let _ = tx
                        .send(AppEvent::TrailingExitDispatchDone {
                            token_id: token_id.clone(),
                            success:  false,
                            error:    Some(format!("trailing exit: {msg} ({attempt_label})")),
                        })
                        .await;
                    return;
                }
                if otype != OrderType::Fak {
                    let _ = tx
                        .send(AppEvent::TrailingExitDispatchDone {
                            token_id: token_id.clone(),
                            success:  false,
                            error:    Some("trailing exit: internal — expected FAK SELL".into()),
                        })
                        .await;
                    return;
                }
                if let Some((ack_qty, ack_price)) =
                    resp.fak_fill_for_position_ack(Side::Sell, shares, price)
                {
                    let _ = tx
                        .send(AppEvent::OrderAck {
                            side:  Side::Sell,
                            outcome,
                            qty:   ack_qty,
                            price: ack_price,
                            clob_order_id: resp.order_id.clone(),
                            token_id:      token_id.clone(),
                        })
                        .await;
                    spawn_open_orders_refresh(
                        trading.clone(),
                        tx.clone(),
                        market_for_refresh.clone(),
                        user_open_ledger.clone(),
                    );
                    let cli2 = trading.clone();
                    let tid = token_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = cli2.refresh_conditional_balance_allowance_cache(&tid).await
                        {
                            debug!(
                                error = %e,
                                token_id = %tid,
                                "post-SELL delayed balance-allowance cache refresh failed (trailing exit)"
                            );
                        }
                    });
                    let _ = tx
                        .send(AppEvent::TrailingExitDispatchDone {
                            token_id: token_id.clone(),
                            success:  true,
                            error:    None,
                        })
                        .await;
                    return;
                } else {
                    if attempt < TRAILING_EXIT_FAK_ATTEMPTS {
                        debug!(
                            %attempt_label,
                            outcome = ?outcome,
                            "trailing exit: no FAK position ack; retry"
                        );
                        continue;
                    }
                    let _ = tx
                        .send(AppEvent::TrailingExitDispatchDone {
                            token_id: token_id.clone(),
                            success:  false,
                            error:    Some(
                                "trailing exit: FAK SELL — no fill ack (see logs)".to_string(),
                            ),
                        })
                        .await;
                    return;
                }
            }
            Err(e) => {
                if attempt < TRAILING_EXIT_FAK_ATTEMPTS {
                    warn!(%attempt_label, error = %e, outcome = ?outcome, "trailing exit: place_order; retry");
                    continue;
                }
                let _ = tx
                    .send(AppEvent::TrailingExitDispatchDone {
                        token_id: token_id.clone(),
                        success:  false,
                        error:    Some(format!("trailing exit: {e} ({attempt_label})")),
                    })
                    .await;
            }
        }
    }
}

use tracing::{debug, error, info, warn};
use trading::{OrderArgs, OrderType, Side, TradingClient};

#[tokio::main]
async fn main() -> Result<()> {
    // ── setup ────────────────────────────────────────────────────────
    // stderr gets hidden once crossterm enters the alternate screen, and
    // any warn! / error! we emit during the TUI phase vanishes. Write the
    // log to ./polymarket-crypto.log so it's always inspectable post-mortem.
    let log_path = std::env::var("POLYMARKET_CRYPTO_LOG_PATH").unwrap_or_else(|_| "./polymarket-crypto.log".into());
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context(format!("open log file {log_path}"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "POLYMARKET_CRYPTO=debug,warn".into())
        )
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    eprintln!("▸ polymarket-crypto — logging to {log_path}");
    eprintln!("▸ if credentials or data don't appear, run: polymarket-crypto debug-auth");

    let cfg = Config::from_env().context("loading config")?;
    info!(
        signer = %cfg.signer_address,
        funder = %cfg.funder,
        proxy  = %net::proxy_env().as_deref().unwrap_or("<none>"),
        market_buy_take_profit_bps = cfg.market_buy_take_profit_bps,
        market_buy_trail_bps = cfg.market_buy_trail_bps,
        "config loaded (GTD take-profit if TP_BPS>0 and TRAIL=0; trailing if TRAIL_BPS>0; trail arm when mid >= entry×(1+TP bps) from position)",
    );

    // ── subcommand dispatch (no TUI) ─────────────────────────────────
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("debug-auth") => {
            let t = TradingClient::new(cfg.clone())?;
            return t.debug_auth_flow().await;
        }
        Some("help") | Some("-h") | Some("--help") => {
            println!("Usage: polymarket-crypto [SUBCOMMAND]\n");
            println!("Without a subcommand, launches the interactive TUI.\n");
            println!("Subcommands:");
            println!("  debug-auth    Run the CLOB L1 auth flow and dump all intermediate");
            println!("                values (useful for debugging 401/403 errors).");
            println!("  help          Show this help.");
            return Ok(());
        }
        _ => {}
    }

    // Shared event channel — generous buffer so bursts from the book WS don't drop
    let (tx, mut rx) = mpsc::channel::<AppEvent>(512);

    let (rtds_sym_tx, rtds_sym_rx) = watch::channel(String::new());

    // Wizard: load Gamma /series (5m) metadata for the asset list.
    {
        let txx = tx.clone();
        tokio::spawn(async move {
            let client = match net::reqwest_client() {
                Ok(c) => c,
                Err(e) => {
                    let _ = txx
                        .send(AppEvent::SeriesListReady(Err(e.to_string())))
                        .await;
                    return;
                }
            };
            let out = match crate::gamma_series::fetch_crypto_series_for_wizard(&client).await {
                Ok(rows) => Ok(rows),
                Err(e) => Err(e.to_string()),
            };
            let _ = txx.send(AppEvent::SeriesListReady(out)).await;
        });
    }

    // ── spawn feeds ──────────────────────────────────────────────────
    spawn_price_feed(tx.clone(), rtds_sym_rx);
    spawn_ticker(tx.clone());
    spawn_key_reader(tx.clone());

    // Shared `TradingClient` (interior `RwLock` for caches + `Mutex` for one-shot creds derive).
    let trading = Arc::new(TradingClient::new(cfg.clone())?);

    // Best-effort: derive creds on startup. On failure we push the full
    // error into the TUI status line AND the log file — stderr alone is
    // invisible once the alternate screen is active.
    {
        let t  = trading.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = t.ensure_creds().await {
                warn!(error = %e, "could not derive CLOB API credentials yet");
                // Send each line of the error separately so the full chain
                // scrolls through the status line — anyhow's chain has rich
                // detail that gets lost if we only show the top-level msg.
                let msg = format!("{e:#}");
                for line in msg.lines().take(6) {
                    let _ = tx.send(AppEvent::OrderErr(line.to_string())).await;
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                }
                let _ = tx.send(AppEvent::OrderErr(
                    "run `polymarket-crypto debug-auth` for the full auth dump".into()
                )).await;
            }
        });
    }

    // On-chain USDC.e cash + claimable (CTF via Multicall3 `aggregate3`); Data API indexes redeemable markets
    // (and neg-risk `currentValue`). Polygon RPC uses a direct HTTP client (no `POLYMARKET_PROXY`).
    {
        let tx = tx.clone();
        let funder = cfg.funder;
        let rpc_url = cfg.polygon_rpc_url.clone();
        let rpc_http = match balances::polygon_rpc_http_client() {
            Ok(c) => Some(c),
            Err(e) => {
                warn!(error = %e, "direct Polygon RPC HTTP client failed — balance panel disabled");
                None
            }
        };
        if let Some(rpc_http) = rpc_http {
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(5));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let data_http = match net::reqwest_client() {
                        Ok(h) => h,
                        Err(e) => {
                            debug!(error = %e, "reqwest client for Data API (balance index)");
                            continue;
                        }
                    };
                    match crate::balances::fetch_balance_panel_usdc(
                        &data_http,
                        &rpc_http,
                        &rpc_url,
                        funder,
                    )
                    .await
                    {
                        Ok((cash, claimable)) => {
                            info!(
                                cash_usdc = cash,
                                claimable_usdc = claimable,
                                funder = %format!("{funder:#x}"),
                                "balance panel: on-chain USDC.e + CTF claimable (Multicall3; neg-risk from Data API)"
                            );
                            let _ = tx
                                .send(AppEvent::BalancePanelLoaded {
                                    cash_usdc: cash,
                                    claimable_usdc: claimable,
                                })
                                .await;
                        }
                        Err(e) => {
                            debug!(error = %e, "on-chain balance panel fetch failed");
                        }
                    }
                }
            });
        }
    }

    // Market discovery: first [`AppEvent::StartTrading`] spawns the task; later starts update profile via watch.
    let (market_tx, mut market_rx) = mpsc::channel::<gamma::ActiveMarket>(8);
    let profile_watch_seed = Arc::new(MarketProfile {
        asset: CRYPTO_ASSETS[0].clone(),
        timeframe: Timeframe::M5,
    });
    let (market_profile_tx, market_profile_rx) = watch::channel(profile_watch_seed);
    let mut market_profile_rx_slot = Some(market_profile_rx);

    let (user_bundle_tx, user_bundle_rx) = watch::channel(UserWsBundle::default());
    let (book_token_tx, book_token_rx) = watch::channel(Vec::<String>::new());

    // When a new market arrives, tear down the old book WS and start a new one.
    let tx_for_books = tx.clone();
    let trading_for_positions = trading.clone();
    let data_api_user = cfg.funder;
    let user_open_ledger = std::sync::Arc::new(feeds::clob_user_ws::UserOpenOrdersLedger::new());
    let user_trade_sync = std::sync::Arc::new(feeds::user_trade_sync::UserTradeSync::new());
    // Supervisor moves a clone; `user_open_ledger` / `user_trade_sync` stay in `main` for TUI.
    let user_open_ledger_for_supervisor = user_open_ledger.clone();
    let user_trade_sync_for_supervisor = user_trade_sync.clone();
    let mut book_token_rx_supervisor = book_token_rx;
    tokio::spawn(async move {
        let mut book_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut orders_poll: Option<tokio::task::JoinHandle<()>> = None;
        let mut holders_poll: Option<tokio::task::JoinHandle<()>> = None;

        // User WS: one long-lived connection; `UserWsBundle` adds extra condition IDs for
        // background trailing fills (main updates the watch after each `apply`).
        let ledger_user_ws = user_open_ledger_for_supervisor.clone();
        let uts = user_trade_sync_for_supervisor.clone();
        let _user_ws = feeds::clob_user_ws::spawn(
            trading_for_positions.fill_wait_registry(),
            trading_for_positions.clone(),
            user_bundle_rx,
            tx_for_books.clone(),
            ledger_user_ws,
            uts,
        );

        let mut ui_book_tokens: Vec<String> = Vec::new();
        let mut last_book_spawn: Vec<String> = Vec::new();

        let mut restart_book_ws =
            |ids: &[String], book_handle: &mut Option<tokio::task::JoinHandle<()>>| {
                if ids.len() == last_book_spawn.len()
                    && ids.iter().zip(last_book_spawn.iter()).all(|(a, b)| a == b)
                {
                    return;
                }
                if let Some(h) = book_handle.take() {
                    h.abort();
                }
                if ids.is_empty() {
                    last_book_spawn.clear();
                    return;
                }
                *book_handle = Some(feeds::clob_ws::spawn(
                    ids.to_vec(),
                    clob_forwarder(tx_for_books.clone()),
                ));
                last_book_spawn = ids.to_vec();
            };

        loop {
            tokio::select! {
                mb = market_rx.recv() => {
                    let Some(m) = mb else { break };
                    if let Some(h) = book_handle.take() { h.abort(); }
                    if let Some(h) = orders_poll.take() { h.abort(); }
                    if let Some(h) = holders_poll.take() { h.abort(); }
                    let uwm = feeds::clob_user_ws::UserWsMarket {
                        condition_id:  m.condition_id.clone(),
                        up_token_id:   m.up_token_id.clone(),
                        down_token_id: m.down_token_id.clone(),
                    };
                    user_open_ledger_for_supervisor.roll_market(&uwm).await;
                    user_trade_sync_for_supervisor.on_market_roll().await;
                    ui_book_tokens = vec![m.up_token_id.clone(), m.down_token_id.clone()];
                    let extra = book_token_rx_supervisor.borrow().clone();
                    let merged = merge_ui_and_extra_book_tokens(&ui_book_tokens, &extra);
                    restart_book_ws(&merged, &mut book_handle);
                    let _ = tx_for_books.send(AppEvent::MarketRoll(m.clone())).await;

            let tw = trading_for_positions.clone();
            let up_pw = m.up_token_id.clone();
            let down_pw = m.down_token_id.clone();
            tokio::spawn(async move {
                let ids = [up_pw.as_str(), down_pw.as_str()];
                if let Err(e) = tw.prewarm_order_context(&ids).await {
                    debug!(error = %e, "CLOB order context prewarm failed");
                }
            });

            let t = trading_for_positions.clone();
            let txp = tx_for_books.clone();
            let up_id = m.up_token_id.clone();
            let down_id = m.down_token_id.clone();
            let condition_id = m.condition_id.clone();
            let user_addr = data_api_user;
            let open_ledger_pos = user_open_ledger_for_supervisor.clone();
            tokio::spawn(async move {
                let (data_api_up, data_api_down) = match net::reqwest_client() {
                    Ok(http) => match crate::data_api::fetch_positions_for_market(
                        &http,
                        user_addr,
                        &condition_id,
                    )
                    .await
                    {
                        Ok(rows) => {
                            crate::data_api::positions_size_avg_for_tokens(&rows, &up_id, &down_id)
                        }
                        Err(e) => {
                            debug!(
                                error = %e,
                                market = %condition_id,
                                "data-api GET /positions (market) failed"
                            );
                            (None, None)
                        }
                    },
                    Err(e) => {
                        debug!(error = %e, "reqwest client for data-api positions");
                        (None, None)
                    }
                };

                let cli = t.clone();
                if let Err(e) = cli.ensure_creds().await {
                    debug!(error = %e, "positions sync skipped: no CLOB creds");
                    return;
                }
                let up = cli
                    .fetch_conditional_balance_shares(&up_id)
                    .await
                    .unwrap_or_else(|e| {
                        debug!(error = %e, token = %up_id, "fetch UP balance failed");
                        0.0
                    });
                let down = cli
                    .fetch_conditional_balance_shares(&down_id)
                    .await
                    .unwrap_or_else(|e| {
                        debug!(error = %e, token = %down_id, "fetch DOWN balance failed");
                        0.0
                    });
                let oo_raw = match cli.fetch_open_orders_for_market(&condition_id).await {
                    Ok(rows) => rows,
                    Err(e) => {
                        debug!(error = %e, market = %condition_id, "fetch /data/orders failed");
                        vec![]
                    }
                };
                let (escrow_up, escrow_down) =
                    escrow_sell_shares_from_clob_orders(&oo_raw, &up_id, &down_id);

                let trades = match cli.fetch_trades_for_market(&condition_id).await {
                    Ok(t) => t,
                    Err(e) => {
                        debug!(error = %e, market = %condition_id, "fetch /data/trades failed; avg entry unknown");
                        vec![]
                    }
                };
                let (position_up, position_down, fills_bootstrap) = hydrate_positions_from_trades(
                    &trades,
                    &up_id,
                    &down_id,
                    up,
                    down,
                    escrow_up,
                    escrow_down,
                    data_api_up,
                    data_api_down,
                );
                let _ = txp
                    .send(AppEvent::PositionsLoaded {
                        position_up,
                        position_down,
                        fills_bootstrap,
                        refresh_status_line: true,
                    })
                    .await;

                let ui_orders = open_ledger_pos
                    .replace_from_rest(&condition_id, &up_id, &down_id, oo_raw)
                    .await;
                let _ = txp
                    .send(AppEvent::OpenOrdersLoaded { orders: ui_orders })
                    .await;
            });

            // Periodic open-orders + full positions replay (5s) — disabled: parallel L2/REST
            // traffic was fighting the authenticated CLOB user WebSocket. Live user-channel docs:
            // https://docs.polymarket.com/developers/CLOB/websocket/user-channel
            //
            // Current sync path: one-shot load on `MarketRoll` (above) + `spawn_open_orders_refresh`
            // after each place/cancel. User WS still drives `FillWaitRegistry` (trade events).
            //
            // `orders_poll` left unused so the block is easy to restore.
            // let t2 = trading_for_positions.clone();
            // let txp2 = tx_for_books.clone();
            // let up2 = m.up_token_id.clone();
            // let down2 = m.down_token_id.clone();
            // let cond2 = m.condition_id.clone();
            // let user_poll = data_api_user;
            // orders_poll = Some(tokio::spawn(async move {
            //     let mut interval = tokio::time::interval(Duration::from_secs(5));
            //     interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            //     interval.tick().await;
            //     loop {
            //         let cli = t2.clone();
            //         if cli.ensure_creds().await.is_err() {
            //             interval.tick().await;
            //             continue;
            //         }
            //         match cli.fetch_open_orders_for_market(&cond2).await {
            //             Ok(rows) => {
            //                 let (data_api_up, data_api_down) = match net::reqwest_client() {
            //                     Ok(http) => match crate::data_api::fetch_positions_for_market(
            //                         &http,
            //                         user_poll,
            //                         &cond2,
            //                     )
            //                     .await
            //                     {
            //                         Ok(api_rows) => crate::data_api::positions_size_avg_for_tokens(
            //                             &api_rows,
            //                             &up2,
            //                             &down2,
            //                         ),
            //                         Err(e) => {
            //                             debug!(
            //                                 error = %e,
            //                                 market = %cond2,
            //                                 "poll: data-api GET /positions (market) failed"
            //                             );
            //                             (None, None)
            //                         }
            //                     }
            //                     Err(e) => {
            //                         debug!(error = %e, "poll: reqwest client for data-api positions");
            //                         (None, None)
            //                     }
            //                 };
            //                 let (escrow_up, escrow_down) =
            //                     escrow_sell_shares_from_clob_orders(&rows, &up2, &down2);
            //                 let up_bal = cli
            //                     .fetch_conditional_balance_shares(&up2)
            //                     .await
            //                     .unwrap_or_else(|e| {
            //                         debug!(error = %e, token = %up2, "poll: UP conditional balance failed");
            //                         0.0
            //                     });
            //                 let down_bal = cli
            //                     .fetch_conditional_balance_shares(&down2)
            //                     .await
            //                     .unwrap_or_else(|e| {
            //                         debug!(error = %e, token = %down2, "poll: DOWN conditional balance failed");
            //                         0.0
            //                     });
            //                 let trades = match cli.fetch_trades_for_market(&cond2).await {
            //                     Ok(t) => t,
            //                     Err(e) => {
            //                         debug!(
            //                             error = %e,
            //                             market = %cond2,
            //                             "poll: /data/trades failed; fills/positions stale until next tick"
            //                         );
            //                         vec![]
            //                     }
            //                 };
            //                 let (position_up, position_down, fills_bootstrap) =
            //                     hydrate_positions_from_trades(
            //                         &trades,
            //                         &up2,
            //                         &down2,
            //                         up_bal,
            //                         down_bal,
            //                         escrow_up,
            //                         escrow_down,
            //                         data_api_up,
            //                         data_api_down,
            //                     );
            //                 let _ = txp2
            //                     .send(AppEvent::PositionsLoaded {
            //                         position_up,
            //                         position_down,
            //                         fills_bootstrap,
            //                         refresh_status_line: false,
            //                     })
            //                     .await;
            //                 let orders = open_orders_from_clob(rows, &up2, &down2);
            //                 let _ = txp2.send(AppEvent::OpenOrdersLoaded { orders }).await;
            //             }
            //             Err(e) => {
            //                 debug!(error = %e, market = %cond2, "poll /data/orders failed");
            //             }
            //         }
            //         interval.tick().await;
            //     }
            // }));

            let tx_holders = tx_for_books.clone();
            let cond_h = m.condition_id.clone();
            let up_h = m.up_token_id.clone();
            let down_h = m.down_token_id.clone();
            holders_poll = Some(tokio::spawn(async move {
                let http = match net::reqwest_client() {
                    Ok(h) => h,
                    Err(e) => {
                        debug!(error = %e, "top holders poll: reqwest client unavailable");
                        return;
                    }
                };
                // Public GET /holders — 1 Hz is typically fine; back off if you see HTTP 429.
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    match crate::data_api::fetch_top_holders_amount_sums(
                        &http,
                        &cond_h,
                        &up_h,
                        &down_h,
                    )
                    .await
                    {
                        Ok((up_sum, down_sum)) => {
                            let _ = tx_holders
                                .send(AppEvent::TopHoldersSentiment { up_sum, down_sum })
                                .await;
                        }
                        Err(e) => {
                            debug!(
                                error = %e,
                                market = %cond_h,
                                "data-api GET /holders failed"
                            );
                        }
                    }
                    interval.tick().await;
                }
            }));
                }
                r = book_token_rx_supervisor.changed() => {
                    if r.is_err() {
                        break;
                    }
                    let extra = book_token_rx_supervisor.borrow().clone();
                    let merged = merge_ui_and_extra_book_tokens(&ui_book_tokens, &extra);
                    restart_book_ws(&merged, &mut book_handle);
                }
            }
        }
    });

    // ── terminal setup ───────────────────────────────────────────────
    enable_raw_mode()?;
    let mut out = stdout();
    // Do not enable mouse capture: we don't handle mouse events, and SGR mouse mode can make some
    // terminals (e.g. embedded IDE terminal) deliver keyboard only after the pane is clicked.
    // Focus events are opt-in (`EnableFocusChange`); without them, `FocusGained` never fires after
    // hide/show and we cannot re-apply raw mode (see crossterm::event module docs).
    //
    // Kitty keyboard protocol (when supported): lock/modifier bits in `KeyEvent.state` and
    // press/repeat/release kinds — see crossterm `examples/event-read.rs` and
    // https://sw.kovidgoyal.net/kitty/keyboard-protocol/
    // Skip on VS Code's terminal: enabling these flags is known to break CapsLock there.
    let can_kbd_enhance = matches!(crossterm::terminal::supports_keyboard_enhancement(), Ok(true));
    let vscode_terminal = std::env::var("TERM_PROGRAM").ok().as_deref() == Some("vscode");
    let use_keyboard_protocol = can_kbd_enhance && !vscode_terminal;
    if use_keyboard_protocol {
        execute!(out, PushKeyboardEnhancementFlags(keyboard_protocol_flags()))?;
        KEYBOARD_PROTOCOL_ACTIVE.store(true, Ordering::Relaxed);
    }
    execute!(out, EnterAlternateScreen, EnableFocusChange)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;

    let mut state = AppState::new(cfg.default_size_usdc, user_trade_sync.clone());
    let mut discovery_spawned = false;
    let _ = user_bundle_tx.send(build_user_ws_bundle(&state));
    let _ = book_token_tx.send(collect_book_watch_token_ids(&state));

    // ── main loop ────────────────────────────────────────────────────
    /// Drain coalesced feed events in one frame so a burst of book updates
    /// does not force one `draw()` per message (that made size-edit feel frozen).
    const MAX_EVENTS_PER_FRAME: usize = 8192;

    let result: Result<()> = loop {
        term.draw(|f| ui::draw(f, &state))?;
        let draw_finished_at = Instant::now();

        let Some(mut ev) = rx.recv().await else { break Ok(()) };

        let mut should_quit = false;
        let mut had_key = false;
        let mut processed = 0usize;
        loop {
            if matches!(ev, AppEvent::Key(_)) {
                had_key = true;
            }
            if apply_app_event(
                ev,
                &mut state,
                &trading,
                &tx,
                &cfg,
                &user_open_ledger,
                &rtds_sym_tx,
                &market_tx,
                &market_profile_tx,
                &mut market_profile_rx_slot,
                &mut discovery_spawned,
                &user_bundle_tx,
                &book_token_tx,
            )
            .await
            {
                should_quit = true;
                break;
            }
            processed += 1;
            if processed >= MAX_EVENTS_PER_FRAME {
                break;
            }
            match rx.try_recv() {
                Ok(next) => ev = next,
                Err(_) => break,
            }
        }

        if should_quit {
            break Ok(());
        }

        // Throttle feed-driven frames so high-frequency Price/Book does not call `draw` on each.
        // `select!` wakes on `recv` during the wait so keys/feed are not blocked for the full gap.
        if !had_key {
            let deadline = draw_finished_at + FEED_REDRAW_MIN;
            if Instant::now() < deadline {
                tokio::select! {
                    biased;
                    maybe_ev = rx.recv() => {
                        let Some(mut ev) = maybe_ev else { break Ok(()) };
                        loop {
                            if apply_app_event(
                                ev,
                                &mut state,
                                &trading,
                                &tx,
                                &cfg,
                                &user_open_ledger,
                                &rtds_sym_tx,
                                &market_tx,
                                &market_profile_tx,
                                &mut market_profile_rx_slot,
                                &mut discovery_spawned,
                                &user_bundle_tx,
                                &book_token_tx,
                            )
                            .await
                            {
                                should_quit = true;
                                break;
                            }
                            match rx.try_recv() {
                                Ok(next) => ev = next,
                                Err(_) => break,
                            }
                        }
                    }
                    _ = tokio::time::sleep_until(deadline.into()) => {}
                }
                if should_quit {
                    break Ok(());
                }
            }
            while let Ok(next) = rx.try_recv() {
                if apply_app_event(
                    next,
                    &mut state,
                    &trading,
                    &tx,
                    &cfg,
                    &user_open_ledger,
                    &rtds_sym_tx,
                    &market_tx,
                    &market_profile_tx,
                    &mut market_profile_rx_slot,
                    &mut discovery_spawned,
                    &user_bundle_tx,
                    &book_token_tx,
                )
                .await
                {
                    should_quit = true;
                    break;
                }
            }
            if should_quit {
                break Ok(());
            }
        }
    };

    // ── teardown ─────────────────────────────────────────────────────
    disable_raw_mode()?;
    if KEYBOARD_PROTOCOL_ACTIVE.load(Ordering::Relaxed) {
        execute!(term.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    execute!(term.backend_mut(), DisableFocusChange, LeaveAlternateScreen)?;
    term.show_cursor()?;

    if let Err(e) = &result { error!(error = %e, "exited with error"); }
    result
}

// ── event forwarders ────────────────────────────────────────────────

fn spawn_price_feed(tx: mpsc::Sender<AppEvent>, rtds_sym_rx: watch::Receiver<String>) {
    let (ptx, mut prx) = mpsc::channel::<feeds::chainlink::PriceTick>(64);
    let _ = feeds::chainlink::spawn(ptx, rtds_sym_rx);
    tokio::spawn(async move {
        while let Some(p) = prx.recv().await {
            // RTDS can outpace the TUI; forward only the latest tick per burst so
            // `Price` does not crowd out `Book` on the shared bounded queue.
            let mut latest = p;
            while let Ok(more) = prx.try_recv() {
                latest = more;
            }
            let _ = tx.try_send(AppEvent::Price(latest));
        }
    });
}

fn clob_forwarder(tx: mpsc::Sender<AppEvent>) -> mpsc::Sender<feeds::clob_ws::BookSnapshot> {
    let (btx, mut brx) = mpsc::channel::<feeds::clob_ws::BookSnapshot>(64);
    tokio::spawn(async move {
        while let Some(b) = brx.recv().await {
            // Collapse bursts so UP/DOWN snapshots both get a chance on the queue.
            let mut latest: HashMap<String, feeds::clob_ws::BookSnapshot> = HashMap::new();
            latest.insert(b.asset_id.clone(), b);
            while let Ok(more) = brx.try_recv() {
                latest.insert(more.asset_id.clone(), more);
            }
            for snap in latest.into_values() {
                let _ = tx.try_send(AppEvent::Book(snap));
            }
        }
    });
    btx
}

fn spawn_ticker(tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(1));
        iv.tick().await;
        loop {
            iv.tick().await;
            if tx.try_send(AppEvent::Tick).is_err() && tx.is_closed() {
                break;
            }
        }
    });
}

/// After minimize/restore or `EventStream` recovery, some terminals drop raw mode even when we stay
/// on the alternate screen — without it, key events never reach crossterm.
fn tty_restore_raw_mode() {
    if let Err(e) = enable_raw_mode() {
        debug!(error = %e, "enable_raw_mode after tty restore (may be ok if not a TTY)");
    }
}

fn spawn_key_reader(tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        loop {
            if tx.is_closed() {
                break;
            }
            match stream.next().await {
                Some(Ok(ev)) => match ev {
                    CtEvent::Key(k) => {
                        // Crossterm only sets `kind` on Unix when the terminal uses the kitty-style
                        // keyboard protocol (`REPORT_EVENT_TYPES`). Some terminals then emit **only**
                        // `Release` for Return — we would never leave `InputMode::EditSize` and quick
                        // trade keys would be swallowed there. Forward `Press`/`Repeat` always, and `Release`
                        // only for Enter (Return).
                        let forward = matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                            || (k.kind == KeyEventKind::Release && k.code == KeyCode::Enter);
                        if !forward {
                            continue;
                        }
                        if matches!(k.code, KeyCode::Char('x') | KeyCode::Char('X')) {
                            info!(
                                modifiers = ?k.modifiers,
                                "key reader: x/X KeyEvent (Press) — forwarding to main loop"
                            );
                        }
                        if tx.send(AppEvent::Key(k)).await.is_err() {
                            break;
                        }
                    }
                    CtEvent::Resize(_, _) => {
                        // Some terminals emit resize when a pane is restored but not `FocusGained`.
                        tty_restore_raw_mode();
                        let _ = tx.try_send(AppEvent::Tick); // redraw; never block crossterm reader
                    }
                    CtEvent::FocusGained => {
                        // After minimize / tab away, some terminals drop raw mode or the event
                        // stream returns transient I/O errors; restore tty state and redraw.
                        tty_restore_raw_mode();
                        if KEYBOARD_PROTOCOL_ACTIVE.load(Ordering::Relaxed) {
                            // Pop/push resyncs Kitty protocol state after focus changes (avoids
                            // stale uppercase after Caps Lock toggled off-window).
                            let mut out = stdout();
                            let _ = execute!(
                                out,
                                PopKeyboardEnhancementFlags,
                                PushKeyboardEnhancementFlags(keyboard_protocol_flags())
                            );
                        }
                        let _ = tx.try_send(AppEvent::Tick);
                    }
                    CtEvent::FocusLost => {}
                    _ => {}
                },
                Some(Err(e)) => {
                    warn!(
                        error = %e,
                        "crossterm event read failed (e.g. terminal hide/show); recreating stream"
                    );
                    tty_restore_raw_mode();
                    stream = EventStream::new();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                None => {
                    warn!("crossterm EventStream ended; recreating");
                    tty_restore_raw_mode();
                    stream = EventStream::new();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    });
}

// ── action dispatch ─────────────────────────────────────────────────

fn forward_order_err(tx: &mpsc::Sender<AppEvent>, msg: String) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(AppEvent::OrderErrModal(msg)).await;
    });
}

/// Fetch redeemable positions and submit CTF redemption via Polymarket Relayer (Safe) when configured.
fn spawn_claim(tx: mpsc::Sender<AppEvent>, cfg: Config) {
    tokio::spawn(async move {
        info!("CTF redeem: task started (key x)");
        let http = match crate::net::reqwest_client() {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(AppEvent::OrderErr(format!("http client: {e:#}"))).await;
                return;
            }
        };
        let positions = match crate::data_api::fetch_redeemable_positions(&http, cfg.funder).await {
            Ok(p) => p,
            Err(e) => {
                let _ = tx
                    .send(AppEvent::OrderErr(format!("data-api positions: {e:#}")))
                    .await;
                return;
            }
        };
        if !positions.iter().any(|p| p.redeemable) {
            let _ = tx
                .send(AppEvent::OrderErr(
                    "нет redeemable-позиций (Data API) — нечего выкупать".into(),
                ))
                .await;
            return;
        }
        match crate::redeem::redeem_resolved_positions(&cfg, &http, &positions).await {
            Ok(summary) => {
                let _ = tx
                    .send(AppEvent::StatusInfo(format!("CTF redeem: {summary}")))
                    .await;
            }
            Err(e) => {
                let _ = tx.send(AppEvent::OrderErr(format!("redeem: {e:#}"))).await;
            }
        }
    });
}

fn dispatch_action(
    action:  Action,
    state:   &AppState,
    trading: &Arc<TradingClient>,
    tx:      &mpsc::Sender<AppEvent>,
    cfg:     &Config,
    user_open_ledger: &std::sync::Arc<feeds::clob_user_ws::UserOpenOrdersLedger>,
) {
    match action {
        Action::StartTrading(p) => {
            let _ = tx.try_send(AppEvent::StartTrading(p));
        }
        Action::None => {}
        // `main` handles `Action::Quit` before calling this; kept for an exhaustive `match`.
        Action::Quit => {}
        Action::ForceMarketRoll  => { /* market watcher polls every 10s; r is a no-op for now */ }
        Action::Claim => {
            spawn_claim(tx.clone(), cfg.clone());
        }
        Action::FetchSolanaDeposit => {
            let tx = tx.clone();
            let funder = cfg.funder;
            tokio::spawn(async move {
                let client = match net::reqwest_client() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(AppEvent::SolanaDepositFailed(e.to_string())).await;
                        return;
                    }
                };
                match bridge_deposit::fetch_svm_deposit_address(&client, funder).await {
                    Ok((svm, min_deposit_usd)) => {
                        let pay_url = bridge_deposit::solana_pay_transfer_url(
                            &svm,
                            bridge_deposit::SOLANA_MAINNET_USDC_MINT,
                        );
                        let qr_unicode = bridge_deposit::svm_address_qr_unicode(&pay_url)
                            .unwrap_or_else(|e| format!("(QR error: {e})"));
                        let _ = tx
                            .send(AppEvent::SolanaDepositFetched {
                                svm_address: svm,
                                qr_unicode,
                                min_deposit_usd,
                            })
                            .await;
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::SolanaDepositFailed(e.to_string())).await;
                    }
                }
            });
        }
        Action::CancelAll        => {
            let t  = trading.clone();
            let tx = tx.clone();
            let open_ledger = user_open_ledger.clone();
            let market = state.market.clone();
            tokio::spawn(async move {
                match t.cancel_all().await {
                    Ok(_) => {
                        let _ = tx
                            .send(AppEvent::StatusInfo("all open orders cancelled".into()))
                            .await;
                        if let Some(m) = market {
                            spawn_open_orders_refresh(
                                t.clone(),
                                tx.clone(),
                                m,
                                open_ledger,
                            );
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::OrderErrModal(e.to_string())).await;
                    }
                }
            });
        }
        Action::PlaceMarket { outcome, side, size_usdc } => {
            let Some(market) = state.market.clone() else {
                forward_order_err(tx, "no active market".into());
                return;
            };
            let Some((shares, price, otype)) =
                resolve_market_order(
                    state,
                    outcome,
                    side,
                    size_usdc,
                    cfg.market_buy_slippage_bps,
                    cfg.market_sell_slippage_bps,
                )
            else {
                forward_order_err(tx, "no book liquidity".into());
                return;
            };
            let buy_notional = matches!(side, Side::Buy).then_some(size_usdc);
            spawn_order(
                trading.clone(),
                user_open_ledger.clone(),
                tx.clone(),
                market,
                outcome,
                side,
                shares,
                price,
                otype,
                buy_notional,
                0,
                cfg.market_buy_take_profit_bps,
                cfg.market_buy_trail_bps,
                false,
            );
        }
        Action::PlaceLimit { outcome, side, price, size_usdc } => {
            let Some(market) = state.market.clone() else {
                forward_order_err(tx, "no active market".into());
                return;
            };
            // Same band as take-profit limit price (`tp_px = clamp_prob(...)` in `spawn_order` path).
            // Tick snap + EIP-712 amounts happen in `TradingClient::place_order` for all GTD orders.
            let price = clamp_prob(price);
            // Limit BUY: `size_usdc` = USDC notional → shares at limit price.
            // Limit SELL: `size_usdc` = shares (field name is historical).
            let shares = match side {
                Side::Buy => (size_usdc / price).max(0.01),
                Side::Sell => size_usdc.max(0.01),
            };
            if shares + 1e-9 < MIN_LIMIT_ORDER_SHARES {
                forward_order_err(
                    tx,
                    format!("limit: min {MIN_LIMIT_ORDER_SHARES} shares (got {shares:.2})"),
                );
                return;
            }
            let buy_notional = matches!(side, Side::Buy).then_some(size_usdc);
            let exp_secs = match gamma::clob_gtd_expiration_secs_one_s_before_window_end(market.closes_at)
            {
                Ok(s) => s,
                Err(e) => {
                    forward_order_err(tx, format!("limit: {e}"));
                    return;
                }
            };
            spawn_order(
                trading.clone(),
                user_open_ledger.clone(),
                tx.clone(),
                market,
                outcome,
                side,
                shares,
                price,
                OrderType::Gtd,
                buy_notional,
                exp_secs,
                0,
                0,
                false,
            );
        }
    }
}

fn spawn_open_orders_refresh(
    trading:      Arc<TradingClient>,
    tx:           mpsc::Sender<AppEvent>,
    market:       gamma::ActiveMarket,
    open_ledger:  std::sync::Arc<feeds::clob_user_ws::UserOpenOrdersLedger>,
) {
    let condition_id = market.condition_id.clone();
    let up_id = market.up_token_id.clone();
    let down_id = market.down_token_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let cli = trading.clone();
        if cli.ensure_creds().await.is_err() {
            return;
        }
        let Ok(rows) = cli.fetch_open_orders_for_market(&condition_id).await else {
            return;
        };
        let orders = open_ledger
            .replace_from_rest(&condition_id, &up_id, &down_id, rows)
            .await;
        let _ = tx.send(AppEvent::OpenOrdersLoaded { orders }).await;
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_order(
    trading:  Arc<TradingClient>,
    user_open_ledger: std::sync::Arc<feeds::clob_user_ws::UserOpenOrdersLedger>,
    tx:       mpsc::Sender<AppEvent>,
    market:   gamma::ActiveMarket,
    outcome:  Outcome,
    side:     Side,
    shares:   f64,
    price:    f64,
    otype:    OrderType,
    buy_notional_usdc: Option<f64>,
    expiration_unix_secs: u64,
    take_profit_bps: u32,
    // Trailing when > 0; `take_profit_bps` is the **gross** bps move vs entry to arm (see
    // `RequestTrailingArm` / `try_promote_pending_trail`).
    trail_bps: u32,
    // If true: GTD limit sell placed after a market buy (take-profit); used for targeted logging.
    is_take_profit_placement: bool,
) {
    tokio::spawn(async move {
        if is_take_profit_placement {
            info!(
                outcome = ?outcome,
                req_shares = shares,
                limit_price = price,
                gtd_expiration_unix_secs = expiration_unix_secs,
                "take-profit GTD: request task started (POST /order next)",
            );
        }
        let market_for_refresh = market;
        let up_token = market_for_refresh.up_token_id.clone();
        let down_token = market_for_refresh.down_token_id.clone();
        let token_id = match outcome {
            Outcome::Up   => market_for_refresh.up_token_id.clone(),
            Outcome::Down => market_for_refresh.down_token_id.clone(),
        };
        let args = OrderArgs {
            token_id,
            side,
            price,
            size: shares,
            neg_risk: market_for_refresh.neg_risk,
            tick_size: market_for_refresh.tick_size.clone(),
            buy_notional_usdc,
            expiration_unix_secs,
            sell_skip_pre_post_settle: is_take_profit_placement,
        };
        let cli = trading.clone();
        match cli.place_order(args, otype).await {
            Ok(resp) => {
                match otype {
                    OrderType::Fak => {
                        debug!(
                            outcome = ?outcome,
                            side = ?side,
                            req_shares = shares,
                            limit_price = price,
                            buy_notional_usdc = ?buy_notional_usdc,
                            take_profit_bps,
                            trail_bps,
                            success = resp.success,
                            status = ?resp.status,
                            order_id = ?resp.order_id,
                            making_amount = ?resp.making_amount,
                            taking_amount = ?resp.taking_amount,
                            error_msg = ?resp.error,
                            parsed_buy_fill = ?match side {
                                Side::Buy => resp.matched_buy_fill_shares_and_avg_price(),
                                Side::Sell => None,
                            },
                            "CLOB FAK (market) order response"
                        );
                    }
                    OrderType::Gtd => {
                        if is_take_profit_placement {
                            info!(
                                outcome = ?outcome,
                                side = ?side,
                                req_shares = shares,
                                limit_price = price,
                                gtd_expiration_unix_secs = expiration_unix_secs,
                                success = resp.success,
                                status = ?resp.status,
                                order_id = ?resp.order_id,
                                making_amount = ?resp.making_amount,
                                taking_amount = ?resp.taking_amount,
                                error_msg = ?resp.error,
                                "CLOB take-profit GTD (limit sell) order response"
                            );
                        } else {
                            debug!(
                                outcome = ?outcome,
                                side = ?side,
                                req_shares = shares,
                                limit_price = price,
                                buy_notional_usdc = ?buy_notional_usdc,
                                gtd_expiration_unix_secs = expiration_unix_secs,
                                success = resp.success,
                                status = ?resp.status,
                                order_id = ?resp.order_id,
                                making_amount = ?resp.making_amount,
                                taking_amount = ?resp.taking_amount,
                                error_msg = ?resp.error,
                                parsed_buy_fill = ?resp.matched_buy_fill_shares_and_avg_price(),
                                "CLOB GTD (limit) order response"
                            );
                        }
                    }
                    _ => {}
                }
                // HTTP 200 may still have `success: false`; resting limits often use `status: "live"`.
                let ok_ui = resp.success
                    || resp.status.as_ref().is_some_and(|s| {
                        s.eq_ignore_ascii_case("matched")
                            || s.eq_ignore_ascii_case("delayed")
                            || s.eq_ignore_ascii_case("live")
                            || s.eq_ignore_ascii_case("open")
                    });
                if ok_ui {
                    let ack = if otype == OrderType::Fak {
                        resp.fak_fill_for_position_ack(side, shares, price)
                    } else {
                        resp.fill_for_position_ack(side, shares, price, otype)
                    };
                    if let Some((ack_qty, ack_price)) = ack
                    {
                        let token_id_ack = match outcome {
                            Outcome::Up => market_for_refresh.up_token_id.clone(),
                            Outcome::Down => market_for_refresh.down_token_id.clone(),
                        };
                        let _ = tx
                            .send(AppEvent::OrderAck {
                                side,
                                outcome,
                                qty: ack_qty,
                                price: ack_price,
                                clob_order_id: resp.order_id.clone(),
                                token_id:      token_id_ack,
                            })
                            .await;
                    } else {
                        let _ = tx
                            .send(AppEvent::StatusInfo(format!(
                                "{} {} limit resting (no fill yet) — see Open Orders",
                                match side {
                                    crate::trading::Side::Buy => "BUY",
                                    crate::trading::Side::Sell => "SELL",
                                },
                                outcome.as_str(),
                            )))
                            .await;
                    }
                    spawn_open_orders_refresh(
                        trading.clone(),
                        tx.clone(),
                        market_for_refresh.clone(),
                        user_open_ledger.clone(),
                    );

                    if matches!(side, Side::Buy | Side::Sell) {
                        let cli = trading.clone();
                        let tid = match outcome {
                            Outcome::Up => market_for_refresh.up_token_id.clone(),
                            Outcome::Down => market_for_refresh.down_token_id.clone(),
                        };
                        let post_label = if matches!(side, Side::Buy) {
                            "post-BUY"
                        } else {
                            "post-SELL"
                        };
                        tokio::spawn(async move {
                            if let Err(e) = cli.refresh_conditional_balance_allowance_cache(&tid).await
                            {
                                debug!(
                                    error = %e,
                                    token_id = %tid,
                                    "{post_label} delayed balance-allowance cache refresh failed"
                                );
                            }
                        });
                    }

                    if take_profit_bps > 0
                        && trail_bps == 0
                        && matches!(side, Side::Buy)
                        && otype == OrderType::Fak
                    {
                        if price >= 0.99 {
                            info!(
                                limit_price = price,
                                outcome = ?outcome,
                                "take-profit: skipped — market FAK limit price >= 0.99",
                            );
                            return;
                        }
                        info!(
                            take_profit_bps,
                            outcome = ?outcome,
                            "take-profit: market BUY succeeded; evaluating GTD limit sell",
                        );
                        let Some((sell_shares, entry_px, amounts_from_api)) =
                            resp.take_profit_fill_for_market_buy(shares, price)
                        else {
                            info!(
                                outcome = ?outcome,
                                success = resp.success,
                                status = ?resp.status,
                                "take-profit: skipped — could not estimate fill (see StatusInfo in TUI)",
                            );
                            let _ = tx
                                .send(AppEvent::StatusInfo(
                                    "take-profit skipped: no fill amounts and order not in an \
                                     executable state (unexpected for FAK)"
                                        .into(),
                                ))
                                .await;
                            return;
                        };
                        if !amounts_from_api {
                            let _ = tx
                                .send(AppEvent::StatusInfo(
                                    "take-profit: CLOB omitted fill amounts; using order size \
                                     and limit price as fill estimate"
                                        .into(),
                                ))
                                .await;
                        }
                        // Limit SELL: same downstream as manual limit — `place_order` snaps to
                        // `orderPriceMinTickSize` and builds amounts (maker/taker micros on tick).
                        let tp_px = clamp_prob(
                            take_profit_limit_price_crypto_after_fees(
                                entry_px,
                                take_profit_bps,
                            ),
                        );
                        if sell_shares + 1e-9 < MIN_LIMIT_ORDER_SHARES {
                            info!(
                                sell_shares,
                                min_shares = MIN_LIMIT_ORDER_SHARES,
                                "take-profit: skipped — fill below min limit order size",
                            );
                            let _ = tx
                                .send(AppEvent::StatusInfo(format!(
                                    "take-profit skipped: fill {sell_shares:.2} sh < min {:.0} sh",
                                    MIN_LIMIT_ORDER_SHARES
                                )))
                                .await;
                        } else {
                            let exp_secs = match gamma::clob_gtd_expiration_secs_one_s_before_window_end(
                                market_for_refresh.closes_at,
                            ) {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!(error = %e, "take-profit: skipped — GTD expiration computation failed");
                                    let _ = tx
                                        .send(AppEvent::OrderErrModal(format!("take-profit limit: {e}")))
                                        .await;
                                    return;
                                }
                            };
                            let mut tp_sell_shares = sell_shares;
                            if let Some(oid) = resp.order_id.as_deref() {
                                match tokio::time::timeout(
                                    USER_WS_TP_FILL_WAIT,
                                    cli.wait_user_channel_buy_fill(oid),
                                )
                                .await
                                {
                                    Ok(Ok(ws_fill)) => {
                                        if ws_fill.size_shares.is_finite()
                                            && ws_fill.size_shares > 1e-9
                                        {
                                            tp_sell_shares = ws_fill.size_shares;
                                            debug!(
                                                shares = ws_fill.size_shares,
                                                status = %ws_fill.status,
                                                asset_id = %ws_fill.asset_id,
                                                order_id = %oid,
                                                "take-profit: size from CLOB user WS trade"
                                            );
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        debug!(
                                            error = %e,
                                            order_id = %oid,
                                            "take-profit: user WS did not deliver fill (using REST estimate)"
                                        );
                                    }
                                    Err(_) => {
                                        debug!(
                                            order_id = %oid,
                                            "take-profit: user WS fill wait timed out (using REST estimate)"
                                        );
                                    }
                                }
                            }
                            info!(
                                tp_sell_shares,
                                entry_px,
                                tp_limit_px = tp_px,
                                gtd_expiration_unix_secs = exp_secs,
                                outcome = ?outcome,
                                "take-profit: scheduling GTD limit SELL (POST /order; retries if balance lags)",
                            );
                            spawn_order(
                                trading.clone(),
                                user_open_ledger.clone(),
                                tx.clone(),
                                market_for_refresh.clone(),
                                outcome,
                                Side::Sell,
                                tp_sell_shares,
                                tp_px,
                                OrderType::Gtd,
                                None,
                                exp_secs,
                                0,
                                0,
                                true,
                            );
                        }
                    }

                    if trail_bps > 0
                        && matches!(side, Side::Buy)
                        && otype == OrderType::Fak
                    {
                        if price >= 0.99 {
                            info!(
                                limit_price = price,
                                outcome = ?outcome,
                                "trailing: skipped — market FAK limit price >= 0.99",
                            );
                        } else {
                            let Some((plan_sell_sh, entry_px, _amounts_from_api)) =
                                resp.take_profit_fill_for_market_buy(shares, price)
                            else {
                                info!(
                                    outcome = ?outcome,
                                    success = resp.success,
                                    status = ?resp.status,
                                    "trailing: skipped — could not estimate fill",
                                );
                                let _ = tx
                                    .send(AppEvent::StatusInfo(
                                        "trailing skipped: no fill estimate (unexpected for FAK)"
                                            .into(),
                                    ))
                                    .await;
                                return;
                            };
                            info!(
                                trail_bps,
                                take_profit_bps,
                                entry_px,
                                outcome = ?outcome,
                                "trailing: market BUY ok; arming when mid >= entry×(1+TP bps) (position avg or fill est.)"
                            );
                            let token_id = match outcome {
                                Outcome::Up => up_token.clone(),
                                Outcome::Down => down_token.clone(),
                            };
                            let mut plan_sh = plan_sell_sh;
                            if let Some(oid) = resp.order_id.as_deref() {
                                match tokio::time::timeout(
                                    USER_WS_TP_FILL_WAIT,
                                    cli.wait_user_channel_buy_fill(oid),
                                )
                                .await
                                {
                                    Ok(Ok(ws_fill)) => {
                                        if ws_fill.size_shares.is_finite()
                                            && ws_fill.size_shares > 1e-9
                                        {
                                            plan_sh = ws_fill.size_shares;
                                            debug!(
                                                shares = ws_fill.size_shares,
                                                order_id = %oid,
                                                "trailing: size from CLOB user WS trade"
                                            );
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        debug!(
                                            error = %e,
                                            order_id = %oid,
                                            "trailing: user WS did not deliver fill (using REST estimate)"
                                        );
                                    }
                                    Err(_) => {
                                        debug!(
                                            order_id = %oid,
                                            "trailing: user WS fill wait timed out (using estimate)"
                                        );
                                    }
                                }
                            }
                            let _ = tx
                                .send(AppEvent::RequestTrailingArm {
                                    outcome,
                                    entry_price: entry_px,
                                    plan_sell_shares: plan_sh,
                                    token_id,
                                    trail_bps,
                                    activation_bps: take_profit_bps,
                                    market:      market_for_refresh.clone(),
                                })
                                .await;
                        }
                    }
                } else {
                    let msg = resp.error.unwrap_or_else(|| format!("status={:?}", resp.status));
                    if is_take_profit_placement {
                        warn!(
                            outcome = ?outcome,
                            side = ?side,
                            order_type = ?otype,
                            success = resp.success,
                            status = ?resp.status,
                            order_id = ?resp.order_id,
                            error_msg = %msg,
                            "CLOB take-profit GTD order rejected (HTTP OK, error in response body)"
                        );
                    } else {
                        warn!(
                            outcome = ?outcome,
                            side = ?side,
                            order_type = ?otype,
                            success = resp.success,
                            status = ?resp.status,
                            order_id = ?resp.order_id,
                            error_msg = %msg,
                            "CLOB order rejected (HTTP OK, error in response body)"
                        );
                    }
                    let _ = tx.send(AppEvent::OrderErrModal(msg)).await;
                }
            }
            Err(e) => {
                if is_take_profit_placement {
                    warn!(
                        error = %e,
                        outcome = ?outcome,
                        side = ?side,
                        req_shares = shares,
                        limit_price = price,
                        gtd_expiration_unix_secs = expiration_unix_secs,
                        "CLOB take-profit GTD order request failed"
                    );
                } else {
                    match otype {
                        OrderType::Fak => {
                            debug!(
                                error = %e,
                                outcome = ?outcome,
                                side = ?side,
                                req_shares = shares,
                                limit_price = price,
                                take_profit_bps,
                                trail_bps,
                                "CLOB FAK (market) order request failed"
                            );
                        }
                        OrderType::Gtd => {
                            debug!(
                                error = %e,
                                outcome = ?outcome,
                                side = ?side,
                                req_shares = shares,
                                limit_price = price,
                                gtd_expiration_unix_secs = expiration_unix_secs,
                                "CLOB GTD (limit) order request failed"
                            );
                        }
                        _ => {}
                    }
                }
                let _ = tx.send(AppEvent::OrderErrModal(e.to_string())).await;
            }
        }
    });
}