//! Polymarket CLOB **user** WebSocket (`/ws/user`) — authenticated `trade` / `order` events.
//!
//! Subscription uses `auth` + `markets` (condition IDs). See
//! <https://docs.polymarket.com/market-data/websocket/user-channel> .
//!
//! After the initial authenticated subscribe, **market changes** use dynamic
//! `{"markets":[…], "operation":"subscribe"|"unsubscribe"}` so the socket stays open.
//!
//! - **`trade`** → [`FillWaitRegistry`] (take-profit fill wait)
//! - **`order`** (PLACEMENT / UPDATE / CANCELLATION) → in-memory open-order ledger, then
//!   `AppEvent::OpenOrdersLoaded` (same as `GET /data/orders` snapshot path)

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::app::{open_orders_from_clob, AppEvent, OpenOrderRow, Outcome};
use crate::take_profit::outcomes_with_duplicate_resting_sells;
use crate::config::CLOB_WS_USER_URL;
use crate::feeds::user_trade_sync::UserTradeSync;
use crate::net;
use crate::trading::{
    canonical_clob_token_id, clob_asset_ids_match, parse_clob_side_str, parse_user_channel_values,
    try_parse_user_channel_trade, ClobOpenOrder, FillWaitRegistry, Side, TradingClient,
    norm_order_id_key,
};

type UserWsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// `watch` payload for the user channel: CLOB `market` = Gamma `conditionId` + outcome token ids.
#[derive(Clone, Default, Debug, Eq, PartialEq)]
pub struct UserWsMarket {
    pub condition_id:  String,
    pub up_token_id:   String,
    pub down_token_id: String,
}

/// User-channel subscription: UI market + extra condition IDs for background trailing fills.
#[derive(Clone, Default, Debug, Eq, PartialEq)]
pub struct UserWsBundle {
    pub active: UserWsMarket,
    pub extras: Vec<UserWsMarket>,
}

/// OPEN orders mirror for the active market, merged from `GET /data/orders` and user `order` WS
/// events (see [Polymarket user channel](https://docs.polymarket.com/developers/CLOB/websocket/user-channel)).
pub struct UserOpenOrdersLedger {
    inner: Mutex<LedgerInner>,
}

struct LedgerInner {
    market: String,
    up:     String,
    down:   String,
    by_id:  HashMap<String, ClobOpenOrder>,
}

impl UserOpenOrdersLedger {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(LedgerInner {
                market: String::new(),
                up:     String::new(),
                down:   String::new(),
                by_id:  HashMap::new(),
            }),
        }
    }

    /// Call in the market supervisor **before** `user_market_tx.send` and the REST snapshot, so
    /// incoming WS messages for the *previous* `market` are ignored and the ledger is empty until
    /// [`Self::replace_from_rest`].
    pub async fn roll_market(&self, m: &UserWsMarket) {
        let mut g = self.inner.lock().await;
        g.market = m.condition_id.clone();
        g.up = m.up_token_id.clone();
        g.down = m.down_token_id.clone();
        g.by_id.clear();
    }

    /// Remove all resting orders immediately (e.g. after a successful `cancel_all`).
    /// Returns the now-empty UI row list so the caller can push `OpenOrdersLoaded`.
    pub async fn clear_resting_orders(&self) -> Vec<crate::app::OpenOrderRow> {
        let mut g = self.inner.lock().await;
        g.by_id.clear();
        crate::app::open_orders_from_clob(vec![], g.up.as_str(), g.down.as_str())
    }

    /// Full replace from `GET /data/orders` (initial market load only).
    pub async fn replace_from_rest(
        &self,
        market: &str,
        up: &str,
        down: &str,
        rows: Vec<ClobOpenOrder>,
    ) -> Vec<OpenOrderRow> {
        let mut g = self.inner.lock().await;
        g.market = market.to_string();
        g.up = up.to_string();
        g.down = down.to_string();
        g.by_id.clear();
        for o in rows {
            let k = norm_order_id_key(&o.id);
            if !k.is_empty() {
                g.by_id.insert(k, o);
            }
        }
        open_orders_from_clob(
            g.by_id.values().cloned().collect(),
            g.up.as_str(),
            g.down.as_str(),
        )
    }

    /// Resting orders for the ledger’s current market — from in-memory state (user WS `order`
    /// events merged with the last REST snapshot). No `GET /data/orders`.
    pub async fn snapshot_clob_orders(&self) -> Vec<ClobOpenOrder> {
        let g = self.inner.lock().await;
        g.by_id.values().cloned().collect()
    }

    /// Normalized CLOB order ids currently in the ledger (keys of [`LedgerInner::by_id`]).
    /// Used to match user-channel `trade` payloads to **our** `maker_orders` leg, not another maker.
    pub async fn normalized_order_keys(&self) -> HashSet<String> {
        let g = self.inner.lock().await;
        g.by_id.keys().cloned().collect()
    }

    /// `norm_order_id_key(order.id)` → side from the last known open-order row (WS + REST).
    ///
    /// Snapshot this **before** [`Self::apply_order_values`] in the same WS batch as a `trade`:
    /// a full fill removes the order from the ledger, but we still need the placed side for fills
    /// when `maker_orders[].side` is missing or disagrees with what you submitted (see
    /// [Polymarket user-channel `trade`](https://docs.polymarket.com/market-data/websocket/user-channel)
    /// — official examples omit `side` on maker legs).
    pub async fn snapshot_order_side_by_norm_id(&self) -> HashMap<String, Side> {
        let g = self.inner.lock().await;
        g.by_id
            .iter()
            .filter_map(|(k, o)| parse_clob_side_str(&o.side).map(|s| (k.clone(), s)))
            .collect()
    }

    /// Same source as [`Self::snapshot_clob_orders`], formatted for the Open Orders panel.
    pub async fn open_orders_ui_snapshot(&self) -> Option<Vec<OpenOrderRow>> {
        let g = self.inner.lock().await;
        if g.market.is_empty() || (g.up.is_empty() && g.down.is_empty()) {
            return None;
        }
        Some(open_orders_from_clob(
            g.by_id.values().cloned().collect(),
            g.up.as_str(),
            g.down.as_str(),
        ))
    }

    /// Optimistically record a just-placed resting order before the WS `order` event arrives.
    /// The real WS event will overwrite this entry with identical data; cancels remove it normally.
    pub async fn insert_resting_order(&self, order: ClobOpenOrder) {
        let k = norm_order_id_key(&order.id);
        if k.is_empty() {
            return;
        }
        let mut g = self.inner.lock().await;
        g.by_id.insert(k, order);
    }

    /// Apply CLOB `order` events; returns new UI rows if anything changed.
    async fn apply_order_values(&self, values: &[Value]) -> Option<Vec<OpenOrderRow>> {
        let mut changed = false;
        for v in values {
            if !is_order_event(v) {
                continue;
            }
            if self.apply_one_order_value(v).await {
                changed = true;
            }
        }
        if !changed {
            return None;
        }
        let g = self.inner.lock().await;
        if g.market.is_empty() || (g.up.is_empty() && g.down.is_empty()) {
            return None;
        }
        Some(open_orders_from_clob(
            g.by_id.values().cloned().collect(),
            g.up.as_str(),
            g.down.as_str(),
        ))
    }

    async fn apply_one_order_value(&self, v: &Value) -> bool {
        let mut g = self.inner.lock().await;
        let mkt = v
            .get("market")
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(ev) = mkt {
            if !markets_eq(ev, g.market.as_str()) {
                return false;
            }
        } else if let Some(a) = jstr2(v, "asset_id", "assetId") {
            if !g.up.is_empty() {
                let t = canonical_clob_token_id(a);
                if t != g.up && t != g.down
                    && !clob_asset_ids_match(a, g.up.as_str())
                    && !clob_asset_ids_match(a, g.down.as_str())
                {
                    return false;
                }
            }
        }
        let raw_id = v
            .get("id")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("order_id").and_then(|x| x.as_str()));
        let key = match raw_id {
            Some(s) => {
                let k = norm_order_id_key(s);
                if k.is_empty() {
                    return false;
                }
                k
            }
            None => return false,
        };
        let kind = v
            .get("type")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_ascii_uppercase());
        if matches!(
            kind.as_deref(),
            Some("CANCELLATION" | "CANCELED" | "CANCELLED" | "EXPIRED")
        ) {
            return g.by_id.remove(&key).is_some();
        }
        if let Some(row) = clob_order_from_user_ws_order_value(v) {
            let k2 = norm_order_id_key(&row.id);
            if k2.is_empty() {
                return false;
            }
            let remove = {
                let orig = row.original_size.parse::<f64>().unwrap_or(f64::NAN);
                let matched = row.size_matched.parse::<f64>().unwrap_or(0.0);
                orig.is_finite() && (orig - matched) <= 1e-9
            };
            if remove {
                return g.by_id.remove(&k2).is_some();
            }
            g.by_id.insert(k2, row);
            true
        } else {
            false
        }
    }
}

fn is_order_event(v: &Value) -> bool {
    v.get("event_type")
        .and_then(|x| x.as_str())
        .or_else(|| v.get("eventType").and_then(|x| x.as_str()))
        .is_some_and(|s| s.eq_ignore_ascii_case("order"))
}

fn markets_eq(a: &str, b: &str) -> bool {
    fn norm_m(s: &str) -> String {
        s.trim()
            .trim_start_matches("0x")
            .to_ascii_lowercase()
    }
    !a.is_empty() && !b.is_empty() && norm_m(a) == norm_m(b)
}

fn jstr2<'a>(v: &'a Value, a: &str, b: &str) -> Option<&'a str> {
    v.get(a)
        .and_then(|x| x.as_str())
        .or_else(|| v.get(b).and_then(|x| x.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Build a [`ClobOpenOrder`]-compatible row from a user-channel `order` message.
fn clob_order_from_user_ws_order_value(v: &Value) -> Option<ClobOpenOrder> {
    let id = jstr2(v, "id", "orderId")?.to_string();
    let asset_id = jstr2(v, "asset_id", "assetId")?.to_string();
    let side = v.get("side").and_then(|x| x.as_str())?.trim().to_string();
    let price = v.get("price").and_then(|x| x.as_str())?.trim().to_string();
    if price.is_empty() {
        return None;
    }
    let original_size = jstr2(v, "original_size", "originalSize")?.to_string();
    let size_matched = jstr2(v, "size_matched", "sizeMatched").unwrap_or("0").to_string();
    Some(ClobOpenOrder {
        id,
        asset_id,
        side,
        price,
        original_size,
        size_matched,
    })
}

pub fn spawn(
    fill_registry: Arc<FillWaitRegistry>,
    trading: Arc<TradingClient>,
    mut market_watch: watch::Receiver<UserWsBundle>,
    app_tx: mpsc::Sender<AppEvent>,
    open_ledger: Arc<UserOpenOrdersLedger>,
    user_trade_sync: Arc<UserTradeSync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let creds = match trading.ensure_creds().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "CLOB user WS: cannot ensure API creds — retry in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let (mut ws, _) = match net::ws_connect(CLOB_WS_USER_URL).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "CLOB user WS connect failed — retry in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            info!(url = %CLOB_WS_USER_URL, "CLOB user WS connected (session)");

            if let Err(e) = run_session(
                &mut ws,
                &creds,
                &fill_registry,
                &mut market_watch,
                &app_tx,
                &open_ledger,
                user_trade_sync.as_ref(),
            )
            .await
            {
                warn!(error = %e, "CLOB user WS session ended — reconnecting in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    })
}

fn bundle_has_markets(b: &UserWsBundle) -> bool {
    !b.active.condition_id.is_empty()
        || b.extras.iter().any(|e| !e.condition_id.is_empty())
}

async fn wait_nonempty_bundle(rx: &mut watch::Receiver<UserWsBundle>) -> Result<()> {
    loop {
        if bundle_has_markets(&rx.borrow()) {
            return Ok(());
        }
        rx.changed()
            .await
            .map_err(|_| anyhow::anyhow!("user WS: market watch sender dropped"))?;
    }
}

fn desired_condition_ids(b: &UserWsBundle) -> HashSet<String> {
    let mut s = HashSet::new();
    if !b.active.condition_id.is_empty() {
        s.insert(b.active.condition_id.clone());
    }
    for e in &b.extras {
        if !e.condition_id.is_empty() {
            s.insert(e.condition_id.clone());
        }
    }
    s
}

fn resolve_trade_outcome(bundle: &UserWsBundle, asset_id: &str) -> Option<Outcome> {
    let token = canonical_clob_token_id(asset_id);
    let token_s = token.as_ref();
    let markets = std::iter::once(&bundle.active).chain(bundle.extras.iter());
    for m in markets {
        if m.condition_id.is_empty() {
            continue;
        }
        if token_s == m.up_token_id
            || clob_asset_ids_match(asset_id, &m.up_token_id)
        {
            return Some(Outcome::Up);
        }
        if token_s == m.down_token_id
            || clob_asset_ids_match(asset_id, &m.down_token_id)
        {
            return Some(Outcome::Down);
        }
    }
    None
}

async fn send_initial_user_sub_multi(
    ws: &mut UserWsStream,
    creds: &crate::trading::ApiCreds,
    markets: &[String],
) -> Result<()> {
    if markets.is_empty() {
        return Ok(());
    }
    let sub = serde_json::json!({
        "type": "user",
        "markets": markets,
        "auth": {
            "apiKey": creds.api_key,
            "secret": creds.secret,
            "passphrase": creds.passphrase,
        },
    });
    ws.send(Message::Text(sub.to_string().into()))
        .await
        .context("user WS initial subscribe send")?;
    Ok(())
}

async fn send_market_operation(
    ws: &mut UserWsStream,
    condition_id: &str,
    operation: &str,
) -> Result<()> {
    let msg = serde_json::json!({
        "markets": [condition_id],
        "operation": operation,
    });
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .with_context(|| format!("user WS {operation} send"))?;
    Ok(())
}

async fn sync_condition_subscriptions(
    ws: &mut UserWsStream,
    creds: &crate::trading::ApiCreds,
    subscribed: &mut HashSet<String>,
    desired: &HashSet<String>,
    handshake_done: &mut bool,
) -> Result<()> {
    for id in subscribed.difference(desired) {
        send_market_operation(ws, id, "unsubscribe").await?;
    }
    subscribed.retain(|id| desired.contains(id));

    if desired.is_empty() {
        *handshake_done = false;
        return Ok(());
    }

    if !*handshake_done {
        let mut ids: Vec<String> = desired.iter().cloned().collect();
        ids.sort();
        send_initial_user_sub_multi(ws, creds, &ids).await?;
        *subscribed = desired.clone();
        *handshake_done = true;
        info!(n = subscribed.len(), "CLOB user WS initial subscribe (multi-market)");
        return Ok(());
    }

    for id in desired.difference(subscribed).cloned().collect::<Vec<_>>() {
        send_market_operation(ws, &id, "subscribe").await?;
        subscribed.insert(id);
    }
    Ok(())
}

async fn run_session(
    ws: &mut UserWsStream,
    creds: &crate::trading::ApiCreds,
    registry: &FillWaitRegistry,
    market_watch: &mut watch::Receiver<UserWsBundle>,
    app_tx: &mpsc::Sender<AppEvent>,
    open_ledger: &UserOpenOrdersLedger,
    user_trade_sync: &UserTradeSync,
) -> Result<()> {
    let mut subscribed: HashSet<String> = HashSet::new();
    let mut handshake_done = false;
    let mut ping = tokio::time::interval(Duration::from_secs(10));
    ping.tick().await;

    let mut logged_first = false;

    loop {
        wait_nonempty_bundle(market_watch).await?;
        let mut desired = desired_condition_ids(&market_watch.borrow());
        sync_condition_subscriptions(
            ws,
            creds,
            &mut subscribed,
            &desired,
            &mut handshake_done,
        )
        .await?;

        tokio::select! {
            r = market_watch.changed() => {
                r.map_err(|_| anyhow::anyhow!("user WS: market watch closed"))?;
                desired = desired_condition_ids(&market_watch.borrow());
                sync_condition_subscriptions(
                    ws,
                    creds,
                    &mut subscribed,
                    &desired,
                    &mut handshake_done,
                )
                .await?;
                continue;
            }
            _ = ping.tick() => {
                let _ = ws.send(Message::Text("PING".into())).await;
            }
            m = ws.next() => {
                let m = match m {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => {
                        info!("CLOB user WS stream ended");
                        return Ok(());
                    }
                };
                let txt = match m {
                    Message::Text(t) => t,
                    Message::Close(f) => {
                        info!(?f, "CLOB user WS peer closed");
                        return Ok(());
                    }
                    _ => continue,
                };
                if txt.trim().eq_ignore_ascii_case("PONG") {
                    continue;
                }
                if !logged_first {
                    let snip: String = txt.chars().take(220).collect();
                    debug!(len = txt.len(), %snip, "CLOB user WS first payload");
                    logged_first = true;
                }
                let values = parse_user_channel_values(&txt);
                // Side for fills: snapshot **before** `order` updates — a full fill removes the resting
                // row, and `maker_orders[].side` is often absent or can disagree with the placed order
                // ([user channel](https://docs.polymarket.com/market-data/websocket/user-channel)).
                let order_sides_snapshot = open_ledger.snapshot_order_side_by_norm_id().await;
                let orders_update = open_ledger.apply_order_values(&values).await;
                let known_order_keys = open_ledger.normalized_order_keys().await;
                registry.dispatch_trades_in_values(&values).await;
                let bundle = market_watch.borrow().clone();
                if bundle_has_markets(&bundle) {
                    for v in &values {
                        if let Some(f) = try_parse_user_channel_trade(
                            v,
                            &known_order_keys,
                            Some(creds.api_key.as_str()),
                            &order_sides_snapshot,
                        ) {
                            let Some(outcome) = resolve_trade_outcome(&bundle, &f.asset_id) else {
                                continue;
                            };
                            let token_id = f.asset_id.clone();
                            if !user_trade_sync
                                .before_ws_user_fill_apply(
                                    &f.clob_trade_id,
                                    &f.order_leg_id,
                                    f.qty,
                                    f.price,
                                )
                                .await
                            {
                                continue;
                            }
                            let _ = app_tx
                                .send(AppEvent::UserChannelFill {
                                    clob_trade_id: f.clob_trade_id,
                                    order_leg_id:  f.order_leg_id,
                                    side:          f.side,
                                    outcome,
                                    token_id,
                                    qty:           f.qty,
                                    price:         f.price,
                                    ts:            f.match_ts,
                                    from_maker_leg: f.from_maker_leg,
                                })
                                .await;
                        }
                    }
                }
                if let Some(orders) = orders_update {
                    let _ = app_tx.send(AppEvent::OpenOrdersLoaded { orders }).await;
                    // Fixed take-profit: if the ledger now shows multiple resting SELL on one outcome,
                    // ask main to merge them into a single GTD (sum of remaining sizes).
                    let active = &bundle.active;
                    if !active.condition_id.is_empty() {
                        let raw = open_ledger.snapshot_clob_orders().await;
                        for outcome in outcomes_with_duplicate_resting_sells(
                            &raw,
                            active.up_token_id.as_str(),
                            active.down_token_id.as_str(),
                        ) {
                            let _ = app_tx
                                .send(AppEvent::MergeTakeProfitRestingSells { outcome })
                                .await;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn user_ws_ledger_place_then_cancel_empties_ui() {
        let l = UserOpenOrdersLedger::new();
        l.roll_market(&UserWsMarket {
            condition_id:  "0xabc1".to_string(),
            up_token_id:   "111".to_string(),
            down_token_id: "222".to_string(),
        })
        .await;
        let place: Value = serde_json::from_str(
            r#"{
            "event_type":"order",
            "id":"0xff01",
            "market":"0xabc1",
            "type":"PLACEMENT",
            "asset_id":"111",
            "side":"BUY",
            "price":"0.5",
            "original_size":"10",
            "size_matched":"0"
        }"#,
        )
        .unwrap();
        let cancel: Value = serde_json::from_str(
            r#"{"event_type":"order","id":"0xff01","market":"0xabc1","type":"CANCELLATION"}"#,
        )
        .unwrap();
        let o = l.apply_order_values(&[place]).await;
        assert!(o.is_some());
        let o2 = l.apply_order_values(&[cancel]).await;
        assert!(o2.is_some());
        let rows = o2.expect("rows");
        assert!(rows.is_empty());
    }
}