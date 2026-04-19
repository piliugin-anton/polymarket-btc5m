//! Application entry point.
//!
//! Boots:
//!   1. Tracing → stderr (filtered by RUST_LOG).
//!   2. Trading client (derives L2 API creds on first use).
//!   3. Chainlink RTDS task (BTC price).
//!   4. Gamma market discovery + auto-roll task.
//!   5. CLOB book subscription task — restarted on each market roll.
//!   6. Crossterm event task.
//!   7. 1-Hz ticker for countdown.
//!
//! All four push into a single `mpsc<AppEvent>`; the main loop drains that,
//! mutates `AppState`, and re-renders.

mod app;
mod config;
mod data_api;
mod redeem;
mod events;
mod feeds;
mod gamma;
mod net;
mod trading;
mod ui;

use anyhow::{Context, Result};
use app::{
    clamp_prob, hydrate_positions_from_trades, open_orders_from_clob, AppEvent, AppState, Outcome,
    resolve_market_order, MIN_LIMIT_ORDER_SHARES,
};
use config::Config;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use events::Action;
use futures_util::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{collections::HashMap, io::stdout, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};
use trading::{OrderArgs, OrderType, Side, TradingClient};

#[tokio::main]
async fn main() -> Result<()> {
    // ── setup ────────────────────────────────────────────────────────
    // stderr gets hidden once crossterm enters the alternate screen, and
    // any warn! / error! we emit during the TUI phase vanishes. Write the
    // log to ./btc5m-bot.log so it's always inspectable post-mortem.
    let log_path = std::env::var("BTC5M_LOG_PATH").unwrap_or_else(|_| "./btc5m-bot.log".into());
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context(format!("open log file {log_path}"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "btc5m_bot=debug,warn".into())
        )
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    eprintln!("▸ btc5m-bot — logging to {log_path}");
    eprintln!("▸ if credentials or data don't appear, run: btc5m-bot debug-auth");

    let cfg = Config::from_env().context("loading config")?;
    info!(
        signer = %cfg.signer_address,
        funder = %cfg.funder,
        proxy  = %net::proxy_env().as_deref().unwrap_or("<none>"),
        "config loaded",
    );

    // ── subcommand dispatch (no TUI) ─────────────────────────────────
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("debug-auth") => {
            let mut t = TradingClient::new(cfg.clone())?;
            return t.debug_auth_flow().await;
        }
        Some("help") | Some("-h") | Some("--help") => {
            println!("Usage: btc5m-bot [SUBCOMMAND]\n");
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

    // ── spawn feeds ──────────────────────────────────────────────────
    spawn_price_feed(tx.clone());
    spawn_ticker(tx.clone());
    spawn_key_reader(tx.clone());

    // Trading client lives behind a Mutex so the order-placement task can
    // mutate it without passing ownership around.
    let trading = Arc::new(Mutex::new(TradingClient::new(cfg.clone())?));

    // Best-effort: derive creds on startup. On failure we push the full
    // error into the TUI status line AND the log file — stderr alone is
    // invisible once the alternate screen is active.
    {
        let t  = trading.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = t.lock().await.ensure_creds().await {
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
                    "run `btc5m-bot debug-auth` for the full auth dump".into()
                )).await;
            }
        });
    }

    // Poll CLOB cash + Data API claimable for the Balance panel (top-right).
    {
        let t = trading.clone();
        let tx = tx.clone();
        let funder = cfg.funder;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.tick().await;
            loop {
                interval.tick().await;
                let mut cli = t.lock().await;
                if cli.ensure_creds().await.is_err() {
                    continue;
                }
                match cli.fetch_collateral_cash_and_cashout_usdc().await {
                    Ok((cash, _allowance)) => {
                        drop(cli);
                        let claimable = match net::reqwest_client() {
                            Ok(http) => {
                                match crate::data_api::fetch_redeemable_positions(&http, funder).await {
                                    Ok(pos) => crate::data_api::sum_claimable_usdc(&pos),
                                    Err(e) => {
                                        debug!(error = %e, "fetch data-api /positions failed");
                                        0.0
                                    }
                                }
                            }
                            Err(e) => {
                                debug!(error = %e, "reqwest client for data-api");
                                0.0
                            }
                        };
                        info!(
                            cash_usdc = cash,
                            claimable_usdc = claimable,
                            funder = %format!("{funder:#x}"),
                            "loaded balance panel: CLOB cash + Data API claimable (redeemable sum)"
                        );
                        let _ = tx
                            .send(AppEvent::BalancePanelLoaded {
                                cash_usdc: cash,
                                claimable_usdc: claimable,
                            })
                            .await;
                    }
                    Err(e) => {
                        drop(cli);
                        debug!(error = %e, "fetch collateral balance failed");
                    }
                }
            }
        });
    }

    // Market discovery + book-subscription supervisor
    let (market_tx, mut market_rx) = mpsc::channel::<gamma::ActiveMarket>(8);
    spawn_market_watcher(tx.clone(), market_tx);

    // When a new market arrives, tear down the old book WS and start a new one.
    let tx_for_books = tx.clone();
    let trading_for_positions = trading.clone();
    tokio::spawn(async move {
        let mut book_handle: Option<tokio::task::JoinHandle<()>> = None;
        let mut orders_poll: Option<tokio::task::JoinHandle<()>> = None;
        while let Some(m) = market_rx.recv().await {
            if let Some(h) = book_handle.take() { h.abort(); }
            if let Some(h) = orders_poll.take() { h.abort(); }
            let token_ids = vec![m.up_token_id.clone(), m.down_token_id.clone()];
            book_handle = Some(feeds::clob_ws::spawn(token_ids, clob_forwarder(tx_for_books.clone())));
            let _ = tx_for_books.send(AppEvent::MarketRoll(m.clone())).await;

            let t = trading_for_positions.clone();
            let txp = tx_for_books.clone();
            let up_id = m.up_token_id.clone();
            let down_id = m.down_token_id.clone();
            let condition_id = m.condition_id.clone();
            tokio::spawn(async move {
                let mut cli = t.lock().await;
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
                let trades = match cli.fetch_trades_for_market(&condition_id).await {
                    Ok(t) => t,
                    Err(e) => {
                        debug!(error = %e, market = %condition_id, "fetch /data/trades failed; avg entry unknown");
                        vec![]
                    }
                };
                let (position_up, position_down, fills_bootstrap) =
                    hydrate_positions_from_trades(&trades, &up_id, &down_id, up, down);
                let _ = txp
                    .send(AppEvent::PositionsLoaded {
                        position_up,
                        position_down,
                        fills_bootstrap,
                    })
                    .await;

                let oo = match cli.fetch_open_orders_for_market(&condition_id).await {
                    Ok(rows) => open_orders_from_clob(rows, &up_id, &down_id),
                    Err(e) => {
                        debug!(error = %e, market = %condition_id, "fetch /data/orders failed");
                        vec![]
                    }
                };
                let _ = txp.send(AppEvent::OpenOrdersLoaded { orders: oo }).await;
            });

            let t2 = trading_for_positions.clone();
            let txp2 = tx_for_books.clone();
            let up2 = m.up_token_id.clone();
            let down2 = m.down_token_id.clone();
            let cond2 = m.condition_id.clone();
            orders_poll = Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(5));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval.tick().await;
                loop {
                    let mut cli = t2.lock().await;
                    if cli.ensure_creds().await.is_err() {
                        drop(cli);
                        interval.tick().await;
                        continue;
                    }
                    match cli.fetch_open_orders_for_market(&cond2).await {
                        Ok(rows) => {
                            let orders = open_orders_from_clob(rows, &up2, &down2);
                            drop(cli);
                            let _ = txp2.send(AppEvent::OpenOrdersLoaded { orders }).await;
                        }
                        Err(e) => {
                            drop(cli);
                            debug!(error = %e, market = %cond2, "poll /data/orders failed");
                        }
                    }
                    interval.tick().await;
                }
            }));
        }
    });

    // ── terminal setup ───────────────────────────────────────────────
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;

    let mut state = AppState::new(cfg.default_size_usdc);

    // ── main loop ────────────────────────────────────────────────────
    /// Drain coalesced feed events in one frame so a burst of book updates
    /// does not force one `draw()` per message (that made size-edit feel frozen).
    const MAX_EVENTS_PER_FRAME: usize = 8192;

    let result: Result<()> = loop {
        term.draw(|f| ui::draw(f, &state))?;

        let Some(mut ev) = rx.recv().await else { break Ok(()) };

        let mut should_quit = false;
        let mut processed = 0usize;
        loop {
            match ev {
                AppEvent::Key(k) => {
                    let action = events::handle_key(&mut state, k);
                    if matches!(action, Action::Quit) {
                        should_quit = true;
                        break;
                    }
                    if matches!(action, Action::Claim) {
                        info!(
                            key_code = ?k.code,
                            modifiers = ?k.modifiers,
                            "TUI: CTF redeem key pressed (dispatching redeem)"
                        );
                    }
                    dispatch_action(action, &state, &trading, &tx, &cfg);
                }
                e => state.apply(e),
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
    };

    // ── teardown ─────────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    term.show_cursor()?;

    if let Err(e) = &result { error!(error = %e, "exited with error"); }
    result
}

// ── event forwarders ────────────────────────────────────────────────

fn spawn_price_feed(tx: mpsc::Sender<AppEvent>) {
    let (ptx, mut prx) = mpsc::channel::<feeds::chainlink::PriceTick>(64);
    feeds::chainlink::spawn(ptx);
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

fn spawn_key_reader(tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(ev)) = stream.next().await {
            match ev {
                CtEvent::Key(k) if k.kind == crossterm::event::KeyEventKind::Press => {
                    if matches!(k.code, KeyCode::Char('x') | KeyCode::Char('X')) {
                        info!(
                            modifiers = ?k.modifiers,
                            "key reader: x/X KeyEvent (Press) — forwarding to main loop"
                        );
                    }
                    if tx.send(AppEvent::Key(k)).await.is_err() { break; }
                }
                CtEvent::Resize(_, _) => {
                    let _ = tx.try_send(AppEvent::Tick); // redraw; never block crossterm reader
                }
                _ => {}
            }
        }
    });
}

fn spawn_market_watcher(
    tx:        mpsc::Sender<AppEvent>,
    market_tx: mpsc::Sender<gamma::ActiveMarket>,
) {
    tokio::spawn(async move {
        let gamma = gamma::GammaClient::new();
        let mut current_slug: Option<String> = None;
        loop {
            match gamma.find_current_btc_5m().await {
                Ok(m) => {
                    if current_slug.as_deref() != Some(&m.slug) {
                        current_slug = Some(m.slug.clone());
                        if market_tx.send(m).await.is_err() { break; }
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::OrderErr(format!("gamma: {e}"))).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

// ── action dispatch ─────────────────────────────────────────────────

fn forward_order_err(tx: &mpsc::Sender<AppEvent>, msg: String) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(AppEvent::OrderErr(msg)).await;
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
    trading: &Arc<Mutex<TradingClient>>,
    tx:      &mpsc::Sender<AppEvent>,
    cfg:     &Config,
) {
    match action {
        Action::None => {}
        // `main` handles `Action::Quit` before calling this; kept for an exhaustive `match`.
        Action::Quit => {}
        Action::ForceMarketRoll  => { /* market watcher polls every 10s; r is a no-op for now */ }
        Action::Claim => {
            spawn_claim(tx.clone(), cfg.clone());
        }
        Action::CancelAll        => {
            let t  = trading.clone();
            let tx = tx.clone();
            let market = state.market.clone();
            tokio::spawn(async move {
                match t.lock().await.cancel_all().await {
                    Ok(_) => {
                        let _ = tx.send(AppEvent::OrderErr("all open orders cancelled".into())).await;
                        if let Some(m) = market {
                            spawn_open_orders_refresh(t.clone(), tx.clone(), m);
                        }
                    }
                    Err(e) => { let _ = tx.send(AppEvent::OrderErr(format!("cancel failed: {e}"))).await; }
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
            );
        }
        Action::PlaceLimit { outcome, side, price, size_usdc } => {
            let Some(market) = state.market.clone() else {
                forward_order_err(tx, "no active market".into());
                return;
            };
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
            );
        }
    }
}

fn spawn_open_orders_refresh(
    trading: Arc<Mutex<TradingClient>>,
    tx: mpsc::Sender<AppEvent>,
    market: gamma::ActiveMarket,
) {
    let condition_id = market.condition_id.clone();
    let up_id = market.up_token_id.clone();
    let down_id = market.down_token_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut cli = trading.lock().await;
        if cli.ensure_creds().await.is_err() {
            return;
        }
        let Ok(rows) = cli.fetch_open_orders_for_market(&condition_id).await else {
            return;
        };
        let orders = open_orders_from_clob(rows, &up_id, &down_id);
        let _ = tx.send(AppEvent::OpenOrdersLoaded { orders }).await;
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_order(
    trading:  Arc<Mutex<TradingClient>>,
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
) {
    tokio::spawn(async move {
        let market_for_refresh = market.clone();
        let token_id = match outcome {
            Outcome::Up   => market.up_token_id,
            Outcome::Down => market.down_token_id,
        };
        let args = OrderArgs {
            token_id,
            side,
            price,
            size: shares,
            neg_risk: market.neg_risk,
            tick_size: market.tick_size.clone(),
            buy_notional_usdc,
            expiration_unix_secs,
        };
        let mut cli = trading.lock().await;
        match cli.place_order(args, otype).await {
            Ok(resp) => {
                // HTTP 200 may still have `success: false`; resting limits often use `status: "live"`.
                let ok_ui = resp.success
                    || resp.status.as_ref().is_some_and(|s| {
                        s.eq_ignore_ascii_case("matched")
                            || s.eq_ignore_ascii_case("live")
                            || s.eq_ignore_ascii_case("open")
                    });
                if ok_ui {
                    let _ = tx.send(AppEvent::OrderAck {
                        side, outcome, qty: shares, price,
                    }).await;
                    spawn_open_orders_refresh(trading.clone(), tx.clone(), market_for_refresh.clone());

                    if take_profit_bps > 0
                        && matches!(side, Side::Buy)
                        && otype == OrderType::Fak
                    {
                        let fill = resp.matched_buy_fill_shares_and_avg_price().or_else(|| {
                            resp.status.as_deref().and_then(|s| {
                                s.eq_ignore_ascii_case("matched").then_some((shares, price))
                            })
                        });
                        let Some((sell_shares, entry_px)) = fill else {
                            return;
                        };
                        let tp_px = clamp_prob(
                            entry_px * (1.0 + take_profit_bps as f64 / 10_000.0),
                        );
                        if sell_shares + 1e-9 < MIN_LIMIT_ORDER_SHARES {
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
                                    let _ = tx
                                        .send(AppEvent::OrderErr(format!("take-profit limit: {e}")))
                                        .await;
                                    return;
                                }
                            };
                            spawn_order(
                                trading.clone(),
                                tx.clone(),
                                market_for_refresh,
                                outcome,
                                Side::Sell,
                                sell_shares,
                                tp_px,
                                OrderType::Gtd,
                                None,
                                exp_secs,
                                0,
                            );
                        }
                    }
                } else {
                    let msg = resp.error.unwrap_or_else(|| format!("status={:?}", resp.status));
                    let _ = tx.send(AppEvent::OrderErr(msg)).await;
                }
            }
            Err(e) => {
                let _ = tx.send(AppEvent::OrderErr(e.to_string())).await;
            }
        }
    });
}