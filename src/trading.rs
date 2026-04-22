//! Trading layer — EIP-712 order construction + CLOB REST submission.
//!
//! EIP-712 order shape follows **`GET https://clob.polymarket.com/version`** (see Polymarket
//! `clob-client-v2` `resolveVersion`): production currently returns **`1`**, i.e. domain
//! `version: "1"` and the V1 `Order` struct (incl. `taker`, `expiration`, `nonce`, `feeRateBps`).
//! When the API reports **`2`**, we use the V2 struct from the migration docs.
//!
//! L1 (EIP-712 wallet sig) is used ONCE to derive API credentials. After that
//! every order-posting request is authed with L2 (HMAC-SHA256 over
//! `timestamp + method + path + body` using the base64-decoded `secret`).

use anyhow::{anyhow, bail, Context, Result};
use alloy_primitives::{Address, U256, B256, hex};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolStruct, Eip712Domain, eip712_domain};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tracing::debug;

use crate::config::{
    Config, CLOB_HOST, CTF_EXCHANGE_V1, CTF_EXCHANGE_V2, NEG_RISK_CTF_EXCHANGE_V1,
    NEG_RISK_CTF_EXCHANGE_V2, POLYGON_CHAIN_ID,
};

// ── EIP-712 Order structs (CLOB V1 vs V2) ───────────────────────────
// EIP-712 `encodeType` must use the literal name **`Order`** (Polymarket `primaryType: "Order"`).
// `sol!` sets that name from the Solidity struct id — `struct OrderV1` would hash as `OrderV1(...)`,
// which breaks verification. Use separate modules so both can be named `Order`.
mod clob_order_v1 {
    use super::*;
    sol! {
        #[derive(Debug)]
        struct Order {
            uint256 salt;
            address maker;
            address signer;
            address taker;
            uint256 tokenId;
            uint256 makerAmount;
            uint256 takerAmount;
            uint256 expiration;
            uint256 nonce;
            uint256 feeRateBps;
            uint8   side;
            uint8   signatureType;
        }
    }
}

mod clob_order_v2 {
    use super::*;
    sol! {
        #[derive(Debug)]
        struct Order {
            uint256 salt;
            address maker;
            address signer;
            uint256 tokenId;
            uint256 makerAmount;
            uint256 takerAmount;
            uint8   side;
            uint8   signatureType;
            uint256 timestamp;
            bytes32 metadata;
            bytes32 builder;
        }
    }
}

fn domain_v1(verifying_contract: Address) -> Eip712Domain {
    eip712_domain! {
        name:              "Polymarket CTF Exchange",
        version:           "1",
        chain_id:          POLYGON_CHAIN_ID,
        verifying_contract: verifying_contract,
    }
}

fn domain_v2(verifying_contract: Address) -> Eip712Domain {
    eip712_domain! {
        name:              "Polymarket CTF Exchange",
        version:           "2",
        chain_id:          POLYGON_CHAIN_ID,
        verifying_contract: verifying_contract,
    }
}

// ── Public types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side { Buy, Sell }

impl Side {
    fn as_u8(self) -> u8 { match self { Side::Buy => 0, Side::Sell => 1 } }
    fn as_str(self) -> &'static str { match self { Side::Buy => "BUY", Side::Sell => "SELL" } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    #[allow(dead_code)] // parity with CLOB; limits use GTD
    Gtc,
    #[allow(dead_code)]
    Fok,
    Fak,
    Gtd,
}

impl OrderType {
    fn as_str(self) -> &'static str { match self {
        OrderType::Gtc => "GTC", OrderType::Fok => "FOK",
        OrderType::Fak => "FAK", OrderType::Gtd => "GTD",
    } }
}

#[derive(Debug, Clone)]
pub struct OrderArgs {
    pub token_id: String,
    pub side:     Side,
    pub price:    f64,     // 0.01..=0.99
    pub size:     f64,     // in shares
    pub neg_risk: bool,
    /// Gamma `orderPriceMinTickSize` as string, e.g. `"0.01"` — drives amount rounding per
    /// `clob-client-v2` `ROUNDING_CONFIG` / `getOrderRawAmounts`.
    pub tick_size: String,
    /// BUY only: USDC notional the user requested. After share ticks, recomputed USDC can fall
    /// below this (e.g. $1 → $0.99); we bump shares until `maker` ≥ this (2-dec floor).
    pub buy_notional_usdc: Option<f64>,
    /// EIP-712 / JSON `expiration`: UTC unix seconds. `0` for GTC / FOK / FAK. For GTD, must be
    /// non-zero (see Polymarket GTD +60s security offset in order docs).
    pub expiration_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCreds {
    #[serde(rename = "apiKey")] pub api_key: String,
    pub secret:     String, // base64
    pub passphrase: String,
}

#[derive(Debug, Deserialize)]
pub struct PostOrderResponse {
    #[serde(default)] pub success:  bool,
    #[serde(default)]
    #[serde(rename = "orderID")]
    pub order_id: Option<String>,
    #[serde(default)] pub status:   Option<String>,
    /// Matched **BUY**: USDC spent (decimal string). Matched **SELL**: shares sold — see API response.
    #[serde(default, rename = "makingAmount")]
    pub making_amount: Option<String>,
    /// Matched **BUY**: shares received (decimal string). Matched **SELL**: USDC — see API response.
    #[serde(default, rename = "takingAmount")]
    pub taking_amount: Option<String>,
    #[serde(default, rename = "errorMsg")]
    pub error:    Option<String>,
}

impl PostOrderResponse {
    /// After a matched **BUY**: `makingAmount` is USDC, `takingAmount` is shares (Polymarket
    /// `SendOrderResponse` decimal strings — **not** scaled by 1e6).
    pub fn matched_buy_fill_shares_and_avg_price(&self) -> Option<(f64, f64)> {
        let mk = self.making_amount.as_ref()?.trim();
        let tk = self.taking_amount.as_ref()?.trim();
        if mk.is_empty() || tk.is_empty() {
            return None;
        }
        let maker_usdc = mk.parse::<f64>().ok()?;
        let taker_sh = tk.parse::<f64>().ok()?;
        if taker_sh <= 1e-12 || !maker_usdc.is_finite() {
            return None;
        }
        Some((taker_sh, maker_usdc / taker_sh))
    }

    /// After a matched **SELL**: `makingAmount` is shares sold, `takingAmount` is USDC received.
    pub fn matched_sell_fill_shares_and_avg_price(&self) -> Option<(f64, f64)> {
        let mk = self.making_amount.as_ref()?.trim();
        let tk = self.taking_amount.as_ref()?.trim();
        if mk.is_empty() || tk.is_empty() {
            return None;
        }
        let sell_sh = mk.parse::<f64>().ok()?;
        let usdc = tk.parse::<f64>().ok()?;
        if sell_sh <= 1e-12 || !usdc.is_finite() {
            return None;
        }
        Some((sell_sh, usdc / sell_sh))
    }

    /// If parsed shares are nonsense vs the submitted order (wrong field order, partial parse),
    /// fall back to request-sized estimates.
    fn parsed_buy_fill_plausible(parsed_sh: f64, req_shares: f64) -> bool {
        if !parsed_sh.is_finite() || parsed_sh <= 0.0 {
            return false;
        }
        if req_shares <= 1e-9 {
            return parsed_sh > 1e-15;
        }
        // Absolute dust when the user clearly submitted a normal-sized ticket.
        if req_shares >= 0.1 && parsed_sh < 1e-3 {
            return false;
        }
        // Parsed share count orders of magnitude below request → wrong scale / swapped fields.
        if req_shares >= 0.05 && parsed_sh / req_shares < 1e-5 {
            return false;
        }
        true
    }

    /// Filled shares + avg price for a **market BUY** (FAK), used to place take-profit.
    ///
    /// Prefer `makingAmount` / `takingAmount`. If those are missing or empty, fall back to the
    /// submitted size and limit when the response indicates execution (`matched`, `delayed`, or
    /// `success`). Returns `None` for resting `live` / `open` (no reliable fill yet).
    ///
    /// Second bool: `true` if amounts came from the API, `false` if estimated from the request.
    pub fn take_profit_fill_for_market_buy(
        &self,
        req_shares: f64,
        req_limit_price: f64,
    ) -> Option<(f64, f64, bool)> {
        if let Some((sh, px)) = self.matched_buy_fill_shares_and_avg_price() {
            if Self::parsed_buy_fill_plausible(sh, req_shares) {
                return Some((sh, px, true));
            }
            // Ignore implausible parse; use request-sized fallback below (same as missing amounts).
        }
        let st = self.status.as_deref().map(|s| s.to_ascii_lowercase());
        match st.as_deref() {
            Some("matched") | Some("delayed") => Some((req_shares, req_limit_price, false)),
            Some("live") | Some("open") => None,
            _ if self.success => Some((req_shares, req_limit_price, false)),
            _ => None,
        }
    }

    /// `(qty, avg_price)` to apply to [`crate::app::AppState`] **only for shares that actually
    /// matched**. Resting limits (`live` / `open`) with no amounts must return `None` so the
    /// Positions panel does not treat unfilled size as inventory.
    pub fn fill_for_position_ack(
        &self,
        side: Side,
        req_shares: f64,
        req_price: f64,
        order_type: OrderType,
    ) -> Option<(f64, f64)> {
        let from_api = match side {
            Side::Buy => self.matched_buy_fill_shares_and_avg_price()
                .filter(|(sh, _)| Self::parsed_buy_fill_plausible(*sh, req_shares)),
            Side::Sell => self.matched_sell_fill_shares_and_avg_price()
                .filter(|(sh, _)| Self::parsed_buy_fill_plausible(*sh, req_shares)),
        };
        if let Some(pair) = from_api {
            return Some(pair);
        }

        let st = self.status.as_deref().map(|s| s.to_ascii_lowercase());
        if matches!(st.as_deref(), Some("live") | Some("open")) {
            return None;
        }

        if matches!(st.as_deref(), Some("matched") | Some("delayed")) {
            return Some((req_shares, req_price));
        }

        if self.success && matches!(order_type, OrderType::Fak | OrderType::Fok) {
            return Some((req_shares, req_price));
        }

        None
    }

    /// Like [`Self::fill_for_position_ack`], but for **FAK/FOK** prefer exchange-reported
    /// `makingAmount` / `takingAmount` even when they diverge from the submitted size (rounding,
    /// USDC-notional estimate vs actual fill). Still rejects obviously swapped fields.
    pub fn fak_fill_for_position_ack(
        &self,
        side: Side,
        req_shares: f64,
        req_price: f64,
    ) -> Option<(f64, f64)> {
        let from_api = match side {
            Side::Buy => self.matched_buy_fill_shares_and_avg_price(),
            Side::Sell => self.matched_sell_fill_shares_and_avg_price(),
        };
        if let Some((sh, px)) = from_api {
            if sh.is_finite()
                && sh > 0.0
                && px.is_finite()
                && px > 0.0
                && !(req_shares >= 0.05 && sh / req_shares < 1e-5)
            {
                return Some((sh, px));
            }
        }
        self.fill_for_position_ack(side, req_shares, req_price, OrderType::Fak)
    }
}

/// `GET /fee-rate` — <https://docs.polymarket.com/api-reference/market-data/get-fee-rate>
#[derive(Debug, Deserialize)]
struct FeeRateResponse {
    #[serde(rename = "base_fee")]
    base_fee: u64,
}

/// One maker leg inside a Polymarket `Trade` (`maker_orders` in REST / TS `MakerOrder`).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClobMakerOrder {
    #[serde(default, rename = "order_id", alias = "orderID", alias = "orderId")]
    pub order_id: String,
    #[serde(default, rename = "matched_amount", alias = "matchedAmount")]
    pub matched_amount: String,
}

/// Polymarket `clob-client-v2` `GET /data/trades` — one row per user fill (L2 auth).
/// Shape matches `Trade` in `clob-client-v2` `types/clob.ts` (extra fields optional for older payloads).
#[derive(Debug, Clone, Deserialize)]
pub struct ClobTrade {
    pub id: String,
    #[serde(alias = "assetId")]
    pub asset_id: String,
    pub side: String,
    pub size: String,
    pub price: String,
    #[serde(alias = "matchTime")]
    pub match_time: String,
    /// When the authenticated user was **taker**, this matches `orderID` from `postOrder`.
    #[serde(default, rename = "taker_order_id", alias = "takerOrderId")]
    pub taker_order_id: Option<String>,
    #[serde(default, rename = "maker_orders", alias = "makerOrders")]
    pub maker_orders: Vec<ClobMakerOrder>,
    /// `"TAKER"` | `"MAKER"` — disambiguates which leg to use with `order_id`.
    #[serde(default, rename = "trader_side", alias = "traderSide")]
    pub trader_side: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TradesPage {
    data: Vec<ClobTrade>,
    #[serde(alias = "nextCursor")]
    next_cursor: String,
}

/// `GET /data/orders` — open orders for the authenticated user (L2 auth), paginated like trades.
#[derive(Debug, Clone, Deserialize)]
pub struct ClobOpenOrder {
    #[serde(default)]
    #[allow(dead_code)] // returned by API; reserved for future (e.g. cancel-by-id)
    pub id: String,
    #[serde(alias = "assetId")]
    pub asset_id: String,
    pub side: String,
    pub price: String,
    #[serde(alias = "originalSize", default)]
    pub original_size: String,
    #[serde(alias = "sizeMatched", default)]
    pub size_matched: String,
}

#[derive(Debug, Deserialize)]
struct OrdersPage {
    data: Vec<ClobOpenOrder>,
    #[serde(alias = "nextCursor")]
    next_cursor: String,
}

/// Cursor sentinels — `clob-client-v2` `constants.ts`.
const TRADES_INITIAL_CURSOR: &str = "MA==";
const TRADES_END_CURSOR: &str = "LTE=";

fn parse_clob_side_str(s: &str) -> Option<Side> {
    match s.trim().to_ascii_uppercase().as_str() {
        "BUY" => Some(Side::Buy),
        "SELL" => Some(Side::Sell),
        _ => None,
    }
}

/// Compare Polymarket ERC-1155 outcome token ids: decimal and `0x` hex denote the same id.
/// Gamma / CLOB / `GET /data/trades` do not always use the same string form; strict `==` misses rows.
pub fn clob_asset_ids_match(a: &str, b: &str) -> bool {
    fn parse_token_u256(s: &str) -> Option<U256> {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            let bytes = ::hex::decode(h).ok()?;
            return Some(U256::from_be_slice(&bytes));
        }
        U256::from_str_radix(t, 10).ok()
    }
    match (parse_token_u256(a), parse_token_u256(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
}

fn norm_order_id_fragment(s: &str) -> String {
    s.trim().trim_start_matches("0x").to_ascii_lowercase()
}

/// Compare Polymarket order ids from `orderID` / `taker_order_id` / `maker_orders.order_id`.
fn order_ids_match(a: &str, b: &str) -> bool {
    let a = norm_order_id_fragment(a);
    let b = norm_order_id_fragment(b);
    !a.is_empty() && a == b
}

/// Sum **BUY** fill size for `token_id` across trades that reference `buy_order_id`
/// ([Polymarket L2 `Trade`](https://docs.polymarket.com/developers/CLOB/clients/methods-l2)).
/// Returns `None` if no matching fill row exists yet.
fn buy_fill_shares_from_trades_for_order(
    trades: &[ClobTrade],
    buy_order_id: &str,
    token_id: &str,
) -> Option<f64> {
    if buy_order_id.trim().is_empty() {
        return None;
    }
    let mut sum = 0.0_f64;
    for t in trades {
        if !clob_asset_ids_match(&t.asset_id, token_id) {
            continue;
        }
        if parse_clob_side_str(&t.side) != Some(Side::Buy) {
            continue;
        }
        let role = t.trader_side.as_deref().map(|s| s.trim().to_ascii_uppercase());

        match role.as_deref() {
            Some("TAKER") => {
                if let Some(ref toid) = t.taker_order_id {
                    if order_ids_match(toid, buy_order_id) {
                        if let Ok(sz) = t.size.parse::<f64>() {
                            if sz.is_finite() && sz > 0.0 {
                                sum += sz;
                            }
                        }
                    }
                }
            }
            Some("MAKER") => {
                for mo in &t.maker_orders {
                    if order_ids_match(&mo.order_id, buy_order_id) {
                        let amt = mo.matched_amount.trim();
                        if amt.is_empty() {
                            continue;
                        }
                        if let Ok(sz) = amt.parse::<f64>() {
                            if sz.is_finite() && sz > 0.0 {
                                sum += sz;
                            }
                        }
                    }
                }
            }
            Some(_) | None => {
                let mut matched_taker = false;
                if let Some(ref toid) = t.taker_order_id {
                    if order_ids_match(toid, buy_order_id) {
                        if let Ok(sz) = t.size.parse::<f64>() {
                            if sz.is_finite() && sz > 0.0 {
                                sum += sz;
                                matched_taker = true;
                            }
                        }
                    }
                }
                if !matched_taker {
                    for mo in &t.maker_orders {
                        if order_ids_match(&mo.order_id, buy_order_id) {
                            let amt = mo.matched_amount.trim();
                            if amt.is_empty() {
                                continue;
                            }
                            if let Ok(sz) = amt.parse::<f64>() {
                                if sz.is_finite() && sz > 0.0 {
                                    sum += sz;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    (sum > 0.0).then_some(sum)
}

/// Net outcome-token shares implied by `GET /data/trades` for this `asset_id` (buys − sells).
fn net_shares_on_token_from_trades(trades: &[ClobTrade], token_id: &str) -> f64 {
    let mut sh = 0.0_f64;
    for t in trades {
        if !clob_asset_ids_match(&t.asset_id, token_id) {
            continue;
        }
        let Some(side) = parse_clob_side_str(&t.side) else {
            continue;
        };
        let Ok(qty) = t.size.parse::<f64>() else {
            continue;
        };
        if !qty.is_finite() || qty <= 0.0 {
            continue;
        }
        match side {
            Side::Buy => sh += qty,
            Side::Sell => sh -= qty,
        }
    }
    sh.max(0.0)
}

/// Reconcile desired take-profit size with **conditional** balance and trade replay (see Polymarket
/// `GET /data/trades`). Avoids insisting on `takingAmount` when it rounds above wallet/trades.
fn take_profit_sell_cap(need: f64, bal: f64, replay: f64) -> f64 {
    const EPS: f64 = 1e-9;
    let bal = if bal.is_finite() { bal.max(0.0) } else { 0.0 };
    let replay = if replay.is_finite() { replay.max(0.0) } else { 0.0 };
    let tol = f64::max(0.02, 0.02 * f64::max(need, f64::max(bal, replay)));

    let sell_cap = match (bal > EPS, replay > EPS) {
        (true, true) => {
            if replay + tol < bal {
                // Trade list not caught up — trust `bal` over stale replay.
                need.min(bal)
            } else if bal + tol < replay {
                // Replay can exceed wallet (fees / rounding / API vs chain). Never size above `bal`.
                need.min(bal).min(replay)
            } else {
                need.min(bal).min(replay)
            }
        }
        (true, false) => need.min(bal),
        (false, true) => need.min(replay),
        (false, false) => 0.0,
    };
    if sell_cap.is_finite() {
        sell_cap.max(0.0)
    } else {
        0.0
    }
}

/// `GET /balance-allowance` — Polymarket `clob-client-v2` `getBalanceAllowance`.
#[derive(Debug, Deserialize)]
struct BalanceAllowanceResponse {
    /// API may return a JSON string or number.
    #[serde(default)]
    balance: Option<serde_json::Value>,
    #[serde(default)]
    allowance: Option<serde_json::Value>,
}

fn json_balance_to_raw(v: &Option<serde_json::Value>) -> u128 {
    match v {
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        Some(serde_json::Value::Number(n)) => n.as_u128().unwrap_or(0),
        _ => 0,
    }
}

fn balance_allowance_error_text(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("not enough balance") || lower.contains("balance / allowance")
}

/// Back-off (ms) before re-reading conditional balance + POST **SELL** on retries. Used after a
/// failed/zero-balance read or a balance/allowance rejection — not on the first FAK attempt (fast path).
const SELL_ORDER_PREP_SETTLE_MS: [u64; 5] = [450, 900, 1_800, 3_000, 5_000];

/// How long `GET /fee-rate` responses are reused per `token_id` (avoids an extra RTT on hot paths).
const FEE_RATE_CACHE_TTL: Duration = Duration::from_secs(600);

struct TradingState {
    creds: Option<ApiCreds>,
    /// Cached `GET /version` → `version` field (1 or 2).
    cached_clob_order_version: Option<u32>,
    /// `(base_fee_bps, fetched_at)` per outcome token id.
    fee_rate_cache: HashMap<String, (u64, Instant)>,
}

// ── Client ──────────────────────────────────────────────────────────

pub struct TradingClient {
    http:   reqwest::Client,
    signer: PrivateKeySigner,
    config: Config,
    state:  RwLock<TradingState>,
    /// Serializes the first L1→L2 credential derivation so concurrent callers don't race.
    creds_derive_lock: AsyncMutex<()>,
}

impl TradingClient {
    pub fn new(config: Config) -> Result<Self> {
        let signer: PrivateKeySigner = config.private_key.parse()
            .context("parsing private key")?;
        Ok(Self {
            http: crate::net::reqwest_client()?,
            signer,
            config,
            state: RwLock::new(TradingState {
                creds: None,
                cached_clob_order_version: None,
                fee_rate_cache: HashMap::new(),
            }),
            creds_derive_lock: AsyncMutex::new(()),
        })
    }

    async fn fetch_clob_order_version(&self) -> Result<u32> {
        {
            let state = self.state.read().await;
            if let Some(v) = state.cached_clob_order_version {
                return Ok(v);
            }
        }
        let url = format!("{CLOB_HOST}/version");
        let resp = self.http.get(&url).send().await.with_context(|| url.clone())?;
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("GET /version failed: {} — {}", status, snip(&txt)));
        }
        #[derive(Deserialize)]
        struct VersionBody {
            version: u32,
        }
        let VersionBody { version } = serde_json::from_str(&txt)
            .with_context(|| format!("decode GET /version: {}", snip(&txt)))?;
        let mut state = self.state.write().await;
        state.cached_clob_order_version = Some(version);
        Ok(version)
    }

    /// Prime `/version` and (if CLOB returns signing v1) `/fee-rate` caches used by
    /// [`Self::place_order`], so the first order avoids cold RTTs.
    pub async fn prewarm_order_context(&self, token_ids: &[&str]) -> Result<()> {
        self.ensure_creds().await?;
        let v = self.fetch_clob_order_version().await?;
        if v == 1 {
            for tid in token_ids {
                let _ = self.fetch_fee_rate_bps(*tid).await?;
            }
        }
        Ok(())
    }

    /// Derive (or create) the L2 API credentials by producing an EIP-712
    /// signature over the `ClobAuth` struct. Tries GET /auth/derive-api-key
    /// first and falls back to POST /auth/api-key if no keys exist yet.
    pub async fn ensure_creds(&self) -> Result<ApiCreds> {
        {
            let state = self.state.read().await;
            if let Some(c) = &state.creds {
                return Ok(c.clone());
            }
        }

        let _derive = self.creds_derive_lock.lock().await;
        {
            let state = self.state.read().await;
            if let Some(c) = &state.creds {
                return Ok(c.clone());
            }
        }

        let ts: i64   = chrono::Utc::now().timestamp();
        let nonce: u64 = 0;
        let auth = self.compute_clob_auth(ts, nonce).await?;

        tracing::debug!(
            addr = %auth.address, ts, nonce,
            digest  = %auth.digest,
            sig     = %auth.signature,
            "L1 auth material computed",
        );

        // 1) Try derive (GET) — returns existing creds if they exist.
        let (get_status, get_body) = self.send_auth(
            reqwest::Method::GET, "/auth/derive-api-key", &auth,
        ).await?;
        tracing::info!(
            status = %get_status,
            body   = %snip(&get_body),
            "GET /auth/derive-api-key",
        );

        if get_status.is_success() {
            let creds: ApiCreds = serde_json::from_str(&get_body)
                .with_context(|| format!("decoding derived creds: {}", snip(&get_body)))?;
            tracing::info!(
                api_key_prefix = &creds.api_key[..std::cmp::min(8, creds.api_key.len())],
                "derived existing CLOB API creds",
            );
            let mut state = self.state.write().await;
            state.creds = Some(creds.clone());
            return Ok(creds);
        }

        // 2) Fallback: create fresh creds.
        let (post_status, post_body) = self.send_auth(
            reqwest::Method::POST, "/auth/api-key", &auth,
        ).await?;
        tracing::info!(
            status = %post_status,
            body   = %snip(&post_body),
            "POST /auth/api-key",
        );

        if post_status.is_success() {
            let creds: ApiCreds = serde_json::from_str(&post_body)
                .with_context(|| format!("decoding created creds: {}", snip(&post_body)))?;
            tracing::info!(
                api_key_prefix = &creds.api_key[..std::cmp::min(8, creds.api_key.len())],
                "created new CLOB API creds",
            );
            let mut state = self.state.write().await;
            state.creds = Some(creds.clone());
            return Ok(creds);
        }

        // Both endpoints rejected — emit the full forensic record at error
        // level so it's visible regardless of RUST_LOG setting.
        tracing::error!(
            get_status   = %get_status,
            get_body     = %get_body,
            post_status  = %post_status,
            post_body    = %post_body,
            address      = %auth.address,
            timestamp    = auth.timestamp,
            digest       = %auth.digest,
            signature    = %auth.signature,
            "CLOB auth rejected by both endpoints",
        );
        Err(anyhow!(
            "CLOB auth failed. GET /auth/derive-api-key → {} \"{}\". POST /auth/api-key → {} \"{}\". Run `polymarket-btc5m debug-auth` for a full dump.",
            get_status, snip(&get_body), post_status, snip(&post_body),
        ))
    }

    /// Send one of the L1-authed auth endpoints. Returns `(status, body)`.
    async fn send_auth(
        &self,
        method: reqwest::Method,
        path:   &str,
        auth:   &ClobAuthDebug,
    ) -> Result<(reqwest::StatusCode, String)> {
        let url = format!("{CLOB_HOST}{path}");
        let req = self.http.request(method, &url)
            .header("POLY_ADDRESS",   &auth.address)
            .header("POLY_SIGNATURE", &auth.signature)
            .header("POLY_TIMESTAMP", auth.timestamp.to_string())
            .header("POLY_NONCE",     auth.nonce.to_string());
        let resp = req.send().await.with_context(|| format!("{url}"))?;
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }

    /// Full breakdown of the L1 auth material. Used by `ensure_creds` and
    /// by the `debug-auth` diagnostic subcommand.
    async fn compute_clob_auth(&self, ts: i64, nonce: u64) -> Result<ClobAuthDebug> {
        use alloy_primitives::keccak256;

        // Domain separator via alloy's helper (the domain struct has no
        // reserved-keyword issue).
        let dom = eip712_domain! {
            name:     "ClobAuthDomain",
            version:  "1",
            chain_id: POLYGON_CHAIN_ID,
        };
        let domain_sep: B256 = dom.separator();

        // `ClobAuth(address address,string timestamp,uint256 nonce,string message)`
        // — computed by hand because `address` is reserved in the sol! macro.
        let type_hash = keccak256(
            b"ClobAuth(address address,string timestamp,uint256 nonce,string message)"
        );
        let ts_str  = ts.to_string();
        let message = "This message attests that I control the given wallet";

        let mut buf = Vec::with_capacity(32 * 5);
        buf.extend_from_slice(type_hash.as_slice());
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(self.signer.address().as_slice());
        buf.extend_from_slice(keccak256(ts_str.as_bytes()).as_slice());
        let mut nonce_be = [0u8; 32];
        nonce_be[24..].copy_from_slice(&nonce.to_be_bytes());
        buf.extend_from_slice(&nonce_be);
        buf.extend_from_slice(keccak256(message.as_bytes()).as_slice());
        let struct_hash = keccak256(&buf);

        let mut digest_in = [0u8; 2 + 32 + 32];
        digest_in[0] = 0x19;
        digest_in[1] = 0x01;
        digest_in[2..34].copy_from_slice(domain_sep.as_slice());
        digest_in[34..].copy_from_slice(struct_hash.as_slice());
        let digest: B256 = keccak256(&digest_in);

        let sig = self.signer.sign_hash(&digest).await?;

        Ok(ClobAuthDebug {
            address:     format!("{:#x}", self.signer.address()),
            timestamp:   ts,
            nonce,
            type_hash:   hex::encode(type_hash),
            domain_sep:  hex::encode(domain_sep),
            struct_hash: hex::encode(struct_hash),
            digest:      hex::encode(digest),
            signature:   format!("0x{}", hex::encode(sig.as_bytes())),
        })
    }

    /// One-shot diagnostic flow — prints every intermediate to stdout and
    /// attempts both auth endpoints, exiting after. Invoked via
    /// `polymarket-btc5m debug-auth`.
    pub async fn debug_auth_flow(&self) -> Result<()> {
        println!("\n━━━ Polymarket CLOB auth diagnostic ━━━\n");

        let signer_addr = self.signer.address();
        let funder = self.config.funder;
        let sig_type = self.config.sig_type as u8;

        println!("signer (EOA) : {signer_addr:#x}");
        println!("funder       : {funder:#x}");
        println!("sig_type     : {sig_type} ({:?})", self.config.sig_type);
        println!("proxy        : {}", crate::net::proxy_env().as_deref().unwrap_or("<none>"));
        println!("CLOB host    : {CLOB_HOST}\n");

        // Sanity checks for common misconfigurations.
        if signer_addr == funder && sig_type != 0 {
            println!("⚠  POLYMARKET_FUNDER equals your signer address but POLYMARKET_SIG_TYPE={sig_type}.");
            println!("   For a plain EOA wallet set POLYMARKET_SIG_TYPE=0.");
            println!("   For a Safe proxy, POLYMARKET_FUNDER must be the Safe address (not the EOA).\n");
        }
        if signer_addr != funder && sig_type == 0 {
            println!("⚠  POLYMARKET_FUNDER differs from your signer but POLYMARKET_SIG_TYPE=0 (EOA).");
            println!("   If the funder is a Gnosis Safe you own, set POLYMARKET_SIG_TYPE=2.\n");
        }

        let ts = chrono::Utc::now().timestamp();
        let auth = self.compute_clob_auth(ts, 0).await?;

        println!("L1 auth material:");
        println!("  timestamp    {}  ({})", auth.timestamp,
            chrono::DateTime::<chrono::Utc>::from_timestamp(auth.timestamp, 0)
                .map(|d| d.to_rfc3339()).unwrap_or_default());
        println!("  nonce        {}", auth.nonce);
        println!("  type_hash    0x{}", auth.type_hash);
        println!("  domain_sep   0x{}", auth.domain_sep);
        println!("  struct_hash  0x{}", auth.struct_hash);
        println!("  digest       0x{}", auth.digest);
        println!("  signature    {}\n", auth.signature);

        // Request 1: GET derive
        println!("→ GET {CLOB_HOST}/auth/derive-api-key");
        let (s, b) = self.send_auth(reqwest::Method::GET, "/auth/derive-api-key", &auth).await?;
        println!("← {} {}", s.as_u16(), s.canonical_reason().unwrap_or(""));
        println!("  {}\n", b.trim());
        if s.is_success() {
            let creds: ApiCreds = serde_json::from_str(&b)?;
            println!("✓ existing credentials derived — you're set up correctly.");
            println!("  apiKey     {}", creds.api_key);
            return Ok(());
        }

        // Request 2: POST create
        println!("→ POST {CLOB_HOST}/auth/api-key");
        let (s2, b2) = self.send_auth(reqwest::Method::POST, "/auth/api-key", &auth).await?;
        println!("← {} {}", s2.as_u16(), s2.canonical_reason().unwrap_or(""));
        println!("  {}\n", b2.trim());
        if s2.is_success() {
            let creds: ApiCreds = serde_json::from_str(&b2)?;
            println!("✓ new credentials created — subsequent runs will derive these.");
            println!("  apiKey     {}", creds.api_key);
            return Ok(());
        }

        // Neither worked. Print a tailored troubleshooting hint.
        println!("✗ both endpoints failed.  Likely causes:");
        println!();
        match s.as_u16() {
            401 => println!("  401 on derive → L1 signature does not recover to POLY_ADDRESS."),
            _ => {}
        }
        match s2.as_u16() {
            401 => println!("  401 on create → L1 signature does not recover to POLY_ADDRESS."),
            403 => println!("  403 on create → wallet is recognised but not permitted (region block?)."),
            404 => println!("  404 on create → endpoint path wrong for your cluster."),
            _ => {}
        }
        if b.contains("timestamp") || b2.contains("timestamp") {
            println!("  Possible clock skew — `sudo ntpdate pool.ntp.org` or equivalent.");
        }
        if b.contains("signature") || b2.contains("signature")
           || b.contains("address") || b2.contains("address") {
            println!("  Signature/address mismatch — verify POLYMARKET_PK corresponds to POLY_ADDRESS.");
        }
        println!("  If you've never used this wallet on polymarket.com, log in there first.");
        println!("  Compare the signature above against py-clob-client with the same ts/nonce.");

        Err(anyhow!("CLOB auth diagnostic failed"))
    }

    async fn fetch_fee_rate_bps(&self, token_id: &str) -> Result<u64> {
        let now = Instant::now();
        {
            let state = self.state.read().await;
            if let Some((bps, t)) = state.fee_rate_cache.get(token_id) {
                if now.duration_since(*t) < FEE_RATE_CACHE_TTL {
                    return Ok(*bps);
                }
            }
        }
        let bps = self.fetch_fee_rate_bps_uncached(token_id).await?;
        let mut state = self.state.write().await;
        if let Some((cached, t)) = state.fee_rate_cache.get(token_id) {
            if Instant::now().duration_since(*t) < FEE_RATE_CACHE_TTL {
                return Ok(*cached);
            }
        }
        state.fee_rate_cache.insert(token_id.to_string(), (bps, Instant::now()));
        Ok(bps)
    }

    async fn fetch_fee_rate_bps_uncached(&self, token_id: &str) -> Result<u64> {
        let mut url = url::Url::parse(&format!("{CLOB_HOST}/fee-rate"))
            .context("parse /fee-rate URL")?;
        url.query_pairs_mut().append_pair("token_id", token_id);
        let resp = self.http
            .get(url.as_str())
            .send()
            .await
            .context("GET /fee-rate")?;
        let status = resp.status();
        let txt = resp.text().await.context("reading /fee-rate body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "CLOB GET /fee-rate failed: {} — {}",
                status,
                snip(&txt)
            ));
        }
        let fr: FeeRateResponse = serde_json::from_str(&txt)
            .with_context(|| format!("decoding fee rate: {}", snip(&txt)))?;
        debug!(
            token_id = %token_id,
            base_fee = fr.base_fee,
            "fetch_fee_rate_bps_uncached"
        );
        Ok(fr.base_fee)
    }

    /// `GET /balance-allowance/update` — asks CLOB to refresh its balance cache before a read.
    /// Matches official `clob-client` `updateBalanceAllowance`; mitigates stale `0` balances
    /// ([clob-client#128](https://github.com/Polymarket/clob-client/issues/128)).
    async fn update_conditional_balance_allowance(&self, token_id: &str) -> Result<()> {
        let creds = self.ensure_creds().await?;
        let ts = chrono::Utc::now().timestamp();
        let path = "/balance-allowance/update";
        let mut url = url::Url::parse(&format!("{CLOB_HOST}{path}"))
            .context("parse /balance-allowance/update URL")?;
        url.query_pairs_mut()
            .append_pair("asset_type", "CONDITIONAL")
            .append_pair("token_id", token_id)
            .append_pair("signature_type", &(self.config.sig_type as u8).to_string());

        let l2_sig = l2_hmac(&creds.secret, ts, "GET", path, "")?;

        let resp = self
            .http
            .get(url.as_str())
            .header("POLY_ADDRESS", format!("{:#x}", self.signer.address()))
            .header("POLY_API_KEY", &creds.api_key)
            .header("POLY_PASSPHRASE", &creds.passphrase)
            .header("POLY_TIMESTAMP", ts.to_string())
            .header("POLY_SIGNATURE", l2_sig)
            .send()
            .await
            .context("GET /balance-allowance/update")?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "CLOB GET /balance-allowance/update failed: {} — {}",
                status,
                snip(&txt)
            ));
        }
        Ok(())
    }

    /// `GET /balance-allowance/update` for conditional tokens — public wrapper for app-level
    /// cache warming (e.g. shortly after a BUY).
    pub async fn refresh_conditional_balance_allowance_cache(&self, token_id: &str) -> Result<()> {
        self.update_conditional_balance_allowance(token_id).await
    }

    /// Conditional (outcome token) balance for `token_id`, in **human shares** (raw / 1e6).
    ///
    /// Uses L2 auth — same as `clob-client-v2` `getBalanceAllowance` with
    /// `asset_type: CONDITIONAL`.
    ///
    /// Sends `GET /balance-allowance/update` first so the read sees a fresher snapshot (Polymarket
    /// `updateBalanceAllowance`). **SELL** placement uses the same GET without that poke (faster path).
    pub async fn fetch_conditional_balance_shares(&self, token_id: &str) -> Result<f64> {
        self.fetch_conditional_balance_shares_impl(token_id, true).await
    }

    async fn fetch_conditional_balance_shares_impl(
        &self,
        token_id: &str,
        refresh_allowance_cache: bool,
    ) -> Result<f64> {
        if refresh_allowance_cache {
            let _ = self.update_conditional_balance_allowance(token_id).await;
        }
        let creds = self.ensure_creds().await?;
        let ts = chrono::Utc::now().timestamp();
        let path = "/balance-allowance";
        let mut url = url::Url::parse(&format!("{CLOB_HOST}{path}"))
            .context("parse /balance-allowance URL")?;
        url.query_pairs_mut()
            .append_pair("asset_type", "CONDITIONAL")
            .append_pair("token_id", token_id)
            .append_pair("signature_type", &(self.config.sig_type as u8).to_string());

        let l2_sig = l2_hmac(&creds.secret, ts, "GET", path, "")?;

        let resp = self.http
            .get(url.as_str())
            .header("POLY_ADDRESS",    format!("{:#x}", self.signer.address()))
            .header("POLY_API_KEY",    &creds.api_key)
            .header("POLY_PASSPHRASE", &creds.passphrase)
            .header("POLY_TIMESTAMP",  ts.to_string())
            .header("POLY_SIGNATURE",  l2_sig)
            .send()
            .await
            .context("GET /balance-allowance")?;
        let status = resp.status();
        let txt = resp.text().await.context("reading /balance-allowance body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "CLOB GET /balance-allowance failed: {} — {}",
                status,
                snip(&txt)
            ));
        }
        let parsed: BalanceAllowanceResponse = serde_json::from_str(&txt)
            .with_context(|| format!("decode /balance-allowance: {}", snip(&txt)))?;
        let raw = json_balance_to_raw(&parsed.balance);
        let shares = raw as f64 / 1_000_000.0;
        debug!(token_id = %token_id, %raw, shares, "fetch_conditional_balance_shares");
        Ok(shares)
    }

    /// After a market **BUY**, wait until we can size a take-profit **SELL** safely.
    ///
    /// Combines `GET /balance-allowance` (CONDITIONAL) with Polymarket `GET /data/trades` so we:
    /// - do not block on `takingAmount` when it is slightly above wallet/trade replay, and
    /// - can proceed when fills appear in trade history before the balance endpoint updates.
    ///
    /// When `buy_order_id` is set (`orderID` from the BUY response), we **require** a matching row
    /// in `/data/trades` (`taker_order_id` / `maker_orders[].order_id`) before sizing, and cap
    /// `want_shares` by that fill sum.
    ///
    /// Returns the **share count to pass into the GTD sell** (≤ `want_shares`).
    pub async fn wait_for_take_profit_sell_shares(
        &self,
        condition_id: &str,
        token_id: &str,
        want_shares: f64,
        min_executable: f64,
        buy_order_id: Option<&str>,
    ) -> Result<f64> {
        const MAX_ATTEMPTS: u32 = 40;
        const DELAY_MS: u64 = 250;
        if !want_shares.is_finite() || want_shares <= 0.0 {
            return self.fetch_conditional_balance_shares_impl(token_id, true).await;
        }
        if !min_executable.is_finite() || min_executable <= 0.0 {
            bail!("min_executable must be positive");
        }
        let need = want_shares;
        let require_trade_for_order = buy_order_id.is_some_and(|s| !s.trim().is_empty());

        for attempt in 0..MAX_ATTEMPTS {
            let bal = self.fetch_conditional_balance_shares_impl(token_id, true).await?;
            let trades = self.fetch_trades_for_market(condition_id).await?;
            let order_fill = match buy_order_id {
                Some(oid) if !oid.trim().is_empty() => {
                    buy_fill_shares_from_trades_for_order(&trades, oid, token_id)
                }
                _ => None,
            };

            if require_trade_for_order && order_fill.is_none() {
                tracing::debug!(
                    attempt,
                    buy_order_id = ?buy_order_id,
                    "take-profit: waiting for BUY orderID to appear in /data/trades"
                );
                tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
                continue;
            }

            let need_for_cap = order_fill
                .map(|f| want_shares.min(f))
                .unwrap_or(want_shares);
            let replay = net_shares_on_token_from_trades(&trades, token_id);
            let sell_cap = take_profit_sell_cap(need_for_cap, bal, replay);

            if sell_cap + 1e-6 >= min_executable {
                if sell_cap + 1e-6 < need {
                    tracing::info!(
                        attempt,
                        want = need,
                        need_for_cap,
                        order_fill,
                        bal,
                        replay,
                        sell_cap,
                        "take-profit: sell size reconciled (order fill + balance / trades replay)"
                    );
                } else if attempt > 0 {
                    tracing::info!(
                        attempt,
                        bal,
                        replay,
                        sell_cap,
                        order_fill,
                        "take-profit: inventory ready for GTD sell (balance + trades)"
                    );
                }
                return Ok(sell_cap);
            }
            tracing::debug!(
                attempt,
                bal,
                replay,
                need,
                need_for_cap,
                order_fill,
                sell_cap,
                min_executable,
                "waiting for take-profit inventory (balance + /data/trades vs min size)"
            );
            tokio::time::sleep(std::time::Duration::from_millis(DELAY_MS)).await;
        }

        let bal = self.fetch_conditional_balance_shares(token_id).await?;
        let trades = self.fetch_trades_for_market(condition_id).await?;
        let order_fill = match buy_order_id {
            Some(oid) if !oid.trim().is_empty() => {
                buy_fill_shares_from_trades_for_order(&trades, oid, token_id)
            }
            _ => None,
        };
        if require_trade_for_order && order_fill.is_none() {
            return Err(anyhow!(
                "take-profit: BUY orderID {:?} not found in /data/trades after ~{}ms",
                buy_order_id,
                u64::from(MAX_ATTEMPTS) * DELAY_MS
            ));
        }
        let need_for_cap = order_fill
            .map(|f| want_shares.min(f))
            .unwrap_or(want_shares);
        let replay = net_shares_on_token_from_trades(&trades, token_id);
        let sell_cap = take_profit_sell_cap(need_for_cap, bal, replay);
        if sell_cap + 1e-6 >= min_executable {
            return Ok(sell_cap);
        }
        Err(anyhow!(
            "take-profit: only {:.6} sh sellable (want {:.6}, bal {:.6}, trades-replay {:.6}, min {:.1}) after ~{}ms",
            sell_cap,
            need,
            bal,
            replay,
            min_executable,
            u64::from(MAX_ATTEMPTS) * DELAY_MS
        ))
    }

    /// USDC **collateral** balance + spending allowance (`GET /balance-allowance`, `asset_type=COLLATERAL`).
    ///
    /// Raw amounts use **6 decimals** (same as conditional shares in this client). The allowance
    /// is the ERC-20 approval the funder granted to Polymarket contracts — often near-unlimited.
    #[allow(dead_code)] // Balance panel uses on-chain USDC.e (`balances`); kept for debugging / scripts.
    pub async fn fetch_collateral_cash_and_cashout_usdc(&self) -> Result<(f64, f64)> {
        let creds = self.ensure_creds().await?;
        let ts = chrono::Utc::now().timestamp();
        let path = "/balance-allowance";
        let mut url = url::Url::parse(&format!("{CLOB_HOST}{path}"))
            .context("parse /balance-allowance URL")?;
        url.query_pairs_mut()
            .append_pair("asset_type", "COLLATERAL")
            .append_pair("signature_type", &(self.config.sig_type as u8).to_string());

        let l2_sig = l2_hmac(&creds.secret, ts, "GET", path, "")?;

        let resp = self
            .http
            .get(url.as_str())
            .header("POLY_ADDRESS", format!("{:#x}", self.signer.address()))
            .header("POLY_API_KEY", &creds.api_key)
            .header("POLY_PASSPHRASE", &creds.passphrase)
            .header("POLY_TIMESTAMP", ts.to_string())
            .header("POLY_SIGNATURE", l2_sig)
            .send()
            .await
            .context("GET /balance-allowance COLLATERAL")?;
        let status = resp.status();
        let txt = resp.text().await.context("reading /balance-allowance body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "CLOB GET /balance-allowance (COLLATERAL) failed: {} — {}",
                status,
                snip(&txt)
            ));
        }
        let parsed: BalanceAllowanceResponse = serde_json::from_str(&txt)
            .with_context(|| format!("decode /balance-allowance COLLATERAL: {}", snip(&txt)))?;
        let raw_bal = json_balance_to_raw(&parsed.balance);
        let raw_allow = json_balance_to_raw(&parsed.allowance);
        let cash = raw_bal as f64 / 1_000_000.0;
        let cashout = raw_allow as f64 / 1_000_000.0;
        debug!(%raw_bal, %raw_allow, cash, cashout, "fetch_collateral_cash_and_cashout_usdc");
        Ok((cash, cashout))
    }

    /// Trades for the authenticated account in `condition_id` (Gamma `conditionId` / CLOB `market`),
    /// paginated like TS `getTrades`.
    ///
    /// **`maker_address` is required** for correct scoping: with only `market`, the CLOB may return
    /// market-wide fills; replaying those produces phantom positions and trade rows (see Polymarket
    /// `TradeParams.maker_address` / py-clob `add_query_trade_params`).
    ///
    /// Use [`Config::funder`] (EIP-712 `Order.maker`), not the EOA [`PrivateKeySigner::address`]:
    /// for `POLY_GNOSIS_SAFE` / `POLY_PROXY` the signer and funder differ; the CLOB indexes fills by
    /// the maker address that actually holds/clears the trade (matches official clients’
    /// `get_trades(TradeParams(maker_address=…))` with the proxy/funder).
    pub async fn fetch_trades_for_market(&self, condition_id: &str) -> Result<Vec<ClobTrade>> {
        let creds = self.ensure_creds().await?;
        let path = "/data/trades";
        let maker = format!("{:#x}", self.config.funder);
        let mut cursor = TRADES_INITIAL_CURSOR.to_string();
        let mut out = Vec::new();
        loop {
            let ts = chrono::Utc::now().timestamp();
            let mut url = url::Url::parse(&format!("{CLOB_HOST}{path}"))
                .context("parse /data/trades URL")?;
            url.query_pairs_mut()
                .append_pair("maker_address", &maker)
                .append_pair("market", condition_id)
                .append_pair("next_cursor", &cursor);
            let l2_sig = l2_hmac(&creds.secret, ts, "GET", path, "")?;
            let resp = self
                .http
                .get(url.as_str())
                .header("POLY_ADDRESS", format!("{:#x}", self.signer.address()))
                .header("POLY_API_KEY", &creds.api_key)
                .header("POLY_PASSPHRASE", &creds.passphrase)
                .header("POLY_TIMESTAMP", ts.to_string())
                .header("POLY_SIGNATURE", l2_sig)
                .send()
                .await
                .context("GET /data/trades")?;
            let status = resp.status();
            let txt = resp.text().await.context("reading /data/trades body")?;
            if !status.is_success() {
                return Err(anyhow!(
                    "CLOB GET /data/trades failed: {} — {}",
                    status,
                    snip(&txt)
                ));
            }
            let page: TradesPage = serde_json::from_str(&txt)
                .with_context(|| format!("decode /data/trades: {}", snip(&txt)))?;
            out.extend(page.data);
            if page.next_cursor == TRADES_END_CURSOR || page.next_cursor.is_empty() {
                break;
            }
            cursor = page.next_cursor;
        }
        debug!(market = %condition_id, n = out.len(), "fetch_trades_for_market");
        Ok(out)
    }

    /// Open orders for `condition_id` (Gamma `conditionId` / CLOB `market`).
    pub async fn fetch_open_orders_for_market(&self, condition_id: &str) -> Result<Vec<ClobOpenOrder>> {
        let creds = self.ensure_creds().await?;
        let path = "/data/orders";
        let mut cursor = TRADES_INITIAL_CURSOR.to_string();
        let mut out = Vec::new();
        loop {
            let ts = chrono::Utc::now().timestamp();
            let mut url = url::Url::parse(&format!("{CLOB_HOST}{path}"))
                .context("parse /data/orders URL")?;
            url.query_pairs_mut()
                .append_pair("market", condition_id)
                .append_pair("next_cursor", &cursor);
            let l2_sig = l2_hmac(&creds.secret, ts, "GET", path, "")?;
            let resp = self
                .http
                .get(url.as_str())
                .header("POLY_ADDRESS", format!("{:#x}", self.signer.address()))
                .header("POLY_API_KEY", &creds.api_key)
                .header("POLY_PASSPHRASE", &creds.passphrase)
                .header("POLY_TIMESTAMP", ts.to_string())
                .header("POLY_SIGNATURE", l2_sig)
                .send()
                .await
                .context("GET /data/orders")?;
            let status = resp.status();
            let txt = resp.text().await.context("reading /data/orders body")?;
            if !status.is_success() {
                return Err(anyhow!(
                    "CLOB GET /data/orders failed: {} — {}",
                    status,
                    snip(&txt)
                ));
            }
            let page: OrdersPage = serde_json::from_str(&txt)
                .with_context(|| format!("decode /data/orders: {}", snip(&txt)))?;
            out.extend(page.data);
            if page.next_cursor == TRADES_END_CURSOR || page.next_cursor.is_empty() {
                break;
            }
            cursor = page.next_cursor;
        }
        debug!(market = %condition_id, n = out.len(), "fetch_open_orders_for_market");
        Ok(out)
    }

    /// Build + sign an order and POST it to the CLOB.
    pub async fn place_order(&self, mut args: OrderArgs, order_type: OrderType)
        -> Result<PostOrderResponse>
    {
        if matches!(order_type, OrderType::Gtc | OrderType::Gtd) {
            args.price = snap_limit_order_price_to_tick(args.price, &args.tick_size);
        }

        // SELL: `GET /balance-allowance` (no preceding `.../update` — see clob-client#128). Market
        // **FAK** skips that read on the first attempt so POST starts immediately; size comes from
        // app state, and retries + clamp recover stale CLOB cache / allowance errors. **GTD** sells
        // keep read+clamp every attempt (min size + maker path).
        let max_post_attempts: u32 = if matches!(args.side, Side::Sell) {
            SELL_ORDER_PREP_SETTLE_MS.len() as u32
        } else {
            1
        };

        for post_attempt in 0..max_post_attempts {
            if matches!(args.side, Side::Sell) {
                let fast_fak_sell =
                    post_attempt == 0 && matches!(order_type, OrderType::Fak);
                if !fast_fak_sell {
                    let settle_idx = post_attempt as usize;
                    let settle_idx = settle_idx.min(SELL_ORDER_PREP_SETTLE_MS.len() - 1);
                    tokio::time::sleep(Duration::from_millis(SELL_ORDER_PREP_SETTLE_MS[settle_idx]))
                        .await;

                    let bal = self
                        .fetch_conditional_balance_shares_impl(&args.token_id, false)
                        .await?;
                    if bal < 1e-6
                        && args.size > 1e-9
                        && post_attempt + 1 < max_post_attempts
                    {
                        tracing::warn!(
                            post_attempt,
                            bal,
                            size = args.size,
                            token_id = %args.token_id,
                            "CLOB conditional balance read 0 after settle; retry before SELL POST"
                        );
                        continue;
                    }
                    if args.size > bal + 1e-12 {
                        tracing::debug!(
                            post_attempt,
                            before = args.size,
                            bal,
                            "SELL size clamped to conditional balance (GET /balance-allowance)"
                        );
                        args.size = bal;
                    }
                }
            }

            let creds = self.ensure_creds().await?;
            let api_version = self.fetch_clob_order_version().await?;
            if order_type == OrderType::Gtd && api_version != 1 {
                bail!(
                    "GTD orders need CLOB signing API version 1 (EIP-712 includes expiration); server returned {api_version}"
                );
            }
            // EIP-712 V1 includes `feeRateBps`; V2 does not — skip `/fee-rate` on the hot path.
            let fee_bps = if api_version == 1 {
                self.fetch_fee_rate_bps(&args.token_id).await?
            } else {
                0u64
            };

            let expiration_u256 = match order_type {
                OrderType::Gtd => {
                    if args.expiration_unix_secs == 0 {
                        bail!("GTD order requires non-zero expiration_unix_secs");
                    }
                    U256::from(args.expiration_unix_secs)
                }
                _ => {
                    if args.expiration_unix_secs != 0 {
                        bail!("expiration_unix_secs must be 0 for {:?}", order_type);
                    }
                    U256::ZERO
                }
            };
            let expiration_wire = match order_type {
                OrderType::Gtd => args.expiration_unix_secs.to_string(),
                _ => "0".into(),
            };

            // 1. Compute maker/taker amounts from price+size (tick-aware — see `getOrderRawAmounts`).
            let (maker_amount, taker_amount) = amounts_for(
                args.side,
                args.size,
                args.price,
                &args.tick_size,
                args.buy_notional_usdc,
                order_type,
            );
            if maker_amount.is_zero() || taker_amount.is_zero() {
                bail!(
                    "order amounts invalid (both must be > 0): maker={maker_amount} taker={taker_amount} \
                     side={} size={} price={} order_type={}",
                    args.side.as_str(),
                    args.size,
                    args.price,
                    order_type.as_str(),
                );
            }

            // 2. Salt — must fit in JS `Number` (see `orderToJsonV1` / `orderToJsonV2` `parseInt`).
            let ts_ms = chrono::Utc::now().timestamp_millis();
            let salt = (rand::random::<f64>() * ts_ms as f64).round() as u64;

            let token_id_u256 = U256::from_str(&args.token_id).context("token_id")?;

            let verifying_v1 = if args.neg_risk { NEG_RISK_CTF_EXCHANGE_V1 } else { CTF_EXCHANGE_V1 };
            let verifying_v2 = if args.neg_risk { NEG_RISK_CTF_EXCHANGE_V2 } else { CTF_EXCHANGE_V2 };

            const PUBLIC_TAKER: &str = "0x0000000000000000000000000000000000000000";
            const ZERO32: &str =
                "0x0000000000000000000000000000000000000000000000000000000000000000";

            // Polymarket TS `orderToJson` / `NewOrder`: there is **no** `price` key on the wire.
            // Tick rules apply to **implied** price from `makerAmount` & `takerAmount` (decimal
            // **strings**, 1e6 token units). `salt` is a JSON **number** (`parseInt` in the client).
            let body = match api_version {
                1 => {
                    // EIP-712 V1 — `exchangeOrderBuilderV1.ts` + `ctfExchangeV1TypedData.ts`
                    let verifying_addr = Address::from_str(verifying_v1)?;
                    let order = clob_order_v1::Order {
                        salt: U256::from(salt),
                        maker: self.config.funder,
                        signer: self.signer.address(),
                        taker: Address::ZERO,
                        tokenId: token_id_u256,
                        makerAmount: maker_amount,
                        takerAmount: taker_amount,
                        expiration: expiration_u256,
                        nonce: U256::ZERO,
                        feeRateBps: U256::from(fee_bps),
                        side: args.side.as_u8(),
                        signatureType: self.config.sig_type as u8,
                    };
                    let dom = domain_v1(verifying_addr);
                    let hash = order.eip712_signing_hash(&dom);
                    let sig = self.signer.sign_hash(&hash).await?;
                    let sig_hex = format!("0x{}", hex::encode(sig.as_bytes()));

                    #[derive(Serialize)]
                    struct OrderPayloadV1<'a> {
                        #[serde(rename = "deferExec")] defer_exec: bool,
                        #[serde(rename = "postOnly")] post_only: bool,
                        order: OrderWireV1,
                        owner: &'a str,
                        #[serde(rename = "orderType")] order_type: &'a str,
                    }
                    #[derive(Serialize)]
                    struct OrderWireV1 {
                        salt: u64,
                        maker: String,
                        signer: String,
                        taker: String,
                        #[serde(rename = "tokenId")] token_id: String,
                        #[serde(rename = "makerAmount")] maker_amount: String,
                        #[serde(rename = "takerAmount")] taker_amount: String,
                        side: String,
                        expiration: String,
                        nonce: String,
                        #[serde(rename = "feeRateBps")] fee_rate_bps: String,
                        #[serde(rename = "signatureType")] signature_type: u8,
                        signature: String,
                    }
                    let payload = OrderPayloadV1 {
                        defer_exec: false,
                        post_only: false,
                        order: OrderWireV1 {
                            salt,
                            maker: format!("{:#x}", order.maker),
                            signer: format!("{:#x}", order.signer),
                            taker: PUBLIC_TAKER.into(),
                            token_id: order.tokenId.to_string(),
                            maker_amount: order.makerAmount.to_string(),
                            taker_amount: order.takerAmount.to_string(),
                            side: args.side.as_str().to_string(),
                            expiration: expiration_wire.clone(),
                            nonce: "0".into(),
                            fee_rate_bps: fee_bps.to_string(),
                            signature_type: self.config.sig_type as u8,
                            signature: sig_hex,
                        },
                        owner: &creds.api_key,
                        order_type: order_type.as_str(),
                    };
                    serde_json::to_string(&payload)?
                }
                2 => {
                    // EIP-712 V2 — `exchangeOrderBuilderV2.ts` + `ctfExchangeV2TypedData.ts`
                    let verifying_addr = Address::from_str(verifying_v2)?;
                    let ts_u256 = U256::from(ts_ms as u128);
                    let order = clob_order_v2::Order {
                        salt: U256::from(salt),
                        maker: self.config.funder,
                        signer: self.signer.address(),
                        tokenId: token_id_u256,
                        makerAmount: maker_amount,
                        takerAmount: taker_amount,
                        side: args.side.as_u8(),
                        signatureType: self.config.sig_type as u8,
                        timestamp: ts_u256,
                        metadata: B256::ZERO,
                        builder: B256::ZERO,
                    };
                    let dom = domain_v2(verifying_addr);
                    let hash = order.eip712_signing_hash(&dom);
                    let sig = self.signer.sign_hash(&hash).await?;
                    let sig_hex = format!("0x{}", hex::encode(sig.as_bytes()));

                    #[derive(Serialize)]
                    struct OrderPayloadV2<'a> {
                        #[serde(rename = "deferExec")] defer_exec: bool,
                        #[serde(rename = "postOnly")] post_only: bool,
                        order: OrderWireV2,
                        owner: &'a str,
                        #[serde(rename = "orderType")] order_type: &'a str,
                    }
                    #[derive(Serialize)]
                    struct OrderWireV2 {
                        salt: u64,
                        maker: String,
                        signer: String,
                        taker: String,
                        #[serde(rename = "tokenId")] token_id: String,
                        #[serde(rename = "makerAmount")] maker_amount: String,
                        #[serde(rename = "takerAmount")] taker_amount: String,
                        side: String,
                        expiration: String,
                        nonce: String,
                        #[serde(rename = "signatureType")] signature_type: u8,
                        signature: String,
                        timestamp: String,
                        metadata: String,
                        builder: String,
                    }
                    let payload = OrderPayloadV2 {
                        defer_exec: false,
                        post_only: false,
                        order: OrderWireV2 {
                            salt,
                            maker: format!("{:#x}", order.maker),
                            signer: format!("{:#x}", order.signer),
                            taker: PUBLIC_TAKER.into(),
                            token_id: order.tokenId.to_string(),
                            maker_amount: order.makerAmount.to_string(),
                            taker_amount: order.takerAmount.to_string(),
                            side: args.side.as_str().to_string(),
                            expiration: expiration_wire,
                            nonce: "0".into(),
                            signature_type: self.config.sig_type as u8,
                            signature: sig_hex,
                            timestamp: ts_ms.to_string(),
                            metadata: ZERO32.into(),
                            builder: ZERO32.into(),
                        },
                        owner: &creds.api_key,
                        order_type: order_type.as_str(),
                    };
                    serde_json::to_string(&payload)?
                }
                v => bail!("CLOB GET /version returned unsupported order version {v}"),
            };

            let post_res = self.post_order_http(&creds, body, order_type).await;
            match post_res {
                Ok(resp) if !resp.success => {
                    let msg = resp.error.as_deref().unwrap_or("");
                    if post_attempt + 1 < max_post_attempts
                        && matches!(args.side, Side::Sell)
                        && balance_allowance_error_text(msg)
                    {
                        tracing::warn!(
                            post_attempt,
                            error = %msg,
                            "CLOB sell rejected (balance/allowance); retry after refresh"
                        );
                        continue;
                    }
                    return Ok(resp);
                }
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let msg = e.to_string();
                    if post_attempt + 1 < max_post_attempts
                        && matches!(args.side, Side::Sell)
                        && balance_allowance_error_text(&msg)
                    {
                        tracing::warn!(
                            post_attempt,
                            error = %msg,
                            "CLOB POST /order failed (balance/allowance); retry after refresh"
                        );
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(anyhow!("internal error: place_order POST loop did not return"))
    }

    async fn post_order_http(
        &self,
        creds: &ApiCreds,
        body: String,
        order_type: OrderType,
    ) -> Result<PostOrderResponse> {
        let ts = chrono::Utc::now().timestamp();
        let path = "/order";
        let l2_sig = l2_hmac(&creds.secret, ts, "POST", path, &body)?;

        let resp = self.http
            .post(format!("{CLOB_HOST}{path}"))
            .header("POLY_ADDRESS",     format!("{:#x}", self.signer.address()))
            .header("POLY_API_KEY",     &creds.api_key)
            .header("POLY_PASSPHRASE",  &creds.passphrase)
            .header("POLY_TIMESTAMP",   ts.to_string())
            .header("POLY_SIGNATURE",   l2_sig)
            .header("Content-Type",     "application/json")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        let txt    = resp.text().await?;
        if !status.is_success() {
            let detail = extract_clob_error_text(&txt)
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(anyhow!("CLOB POST /order: {detail}"));
        }
        if matches!(order_type, OrderType::Fak | OrderType::Gtd) {
            debug!(
                order_type = order_type.as_str(),
                response_body = %snip(&txt),
                "CLOB POST /order raw JSON"
            );
        }
        let out: PostOrderResponse = serde_json::from_str(&txt)
            .with_context(|| format!("decoding order response: {}", snip(&txt)))?;
        Ok(out)
    }

    /// Cancel all open orders for this user.
    pub async fn cancel_all(&self) -> Result<()> {
        let creds = self.ensure_creds().await?;
        let ts = chrono::Utc::now().timestamp();
        let path = "/cancel-all";
        let l2_sig = l2_hmac(&creds.secret, ts, "DELETE", path, "")?;
        let resp = self.http
            .delete(format!("{CLOB_HOST}{path}"))
            .header("POLY_ADDRESS",    format!("{:#x}", self.signer.address()))
            .header("POLY_API_KEY",    &creds.api_key)
            .header("POLY_PASSPHRASE", &creds.passphrase)
            .header("POLY_TIMESTAMP",  ts.to_string())
            .header("POLY_SIGNATURE",  l2_sig)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = extract_clob_error_text(&body)
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(anyhow!("cancel-all: {detail}"));
        }
        Ok(())
    }
}

/// Limit price as **integer** micros (`human * 1e6`), always an exact multiple of the tick in micros.
/// Deriving `price_micros` from `f64` (`raw_price * 1_000_000.0`) can drift (e.g. `0.990115…` vs tick `0.01`)
/// and CLOB rejects with `breaks minimum tick size rule`.
fn snap_limit_price_micros(price: f64, tick: &str) -> u64 {
    let tick_f = tick.trim().parse::<f64>().unwrap_or(0.01);
    if !price.is_finite() || !tick_f.is_finite() || tick_f <= 0.0 {
        return 10_000;
    }
    let tm = (tick_f * 1_000_000.0).round() as u64;
    if tm == 0 {
        return 10_000;
    }
    let clamped_micros = (price.clamp(0.01, 0.99) * 1_000_000.0).round() as i128;
    let tm_i = tm as i128;
    let mut n = ((clamped_micros + tm_i / 2) / tm_i).max(1);
    let min_n = (10_000_i128 + tm_i - 1) / tm_i;
    let max_n = (990_000_i128 / tm_i).max(1);
    n = n.clamp(min_n.min(max_n), max_n);
    (n as u64).saturating_mul(tm)
}

/// CLOB validates limit **implied** prices against `orderPriceMinTickSize` (e.g. 0.01 → two decimals).
/// Call for every GTC/GTD order so `args.price` is never a float tail like `0.7302551640340219`.
fn snap_limit_order_price_to_tick(price: f64, tick: &str) -> f64 {
    snap_limit_price_micros(price, tick) as f64 / 1_000_000.0
}

fn round_cfg_from_tick(tick: &str) -> (u32, u32, u32) {
    let x = tick.trim().parse::<f64>().unwrap_or(0.01);
    if (x - 0.1).abs() < 1e-6 {
        (1, 2, 3)
    } else if (x - 0.01).abs() < 1e-7 {
        (2, 2, 4)
    } else if (x - 0.001).abs() < 1e-8 {
        (3, 2, 5)
    } else if (x - 0.0001).abs() < 1e-9 {
        (4, 2, 6)
    } else {
        (2, 2, 4)
    }
}

/// `decimalPlaces` from `@polymarket/clob-client` `utilities.ts` (approximation for positive sizes).
fn decimal_places_clob(num: f64) -> u32 {
    if !num.is_finite() || num == 0.0 {
        return 0;
    }
    let n = num.abs();
    if (n - n.round()).abs() < 1e-9 {
        return 0;
    }
    let s = format!("{:.12}", n);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if !s.contains('.') {
        return 0;
    }
    s.split('.').nth(1).map(|f| f.len()).unwrap_or(0) as u32
}

/// `roundNormal` — official client short-circuits when `decimalPlaces(num) <= decimals`.
fn round_normal_clob(num: f64, decimals: u32) -> f64 {
    if !num.is_finite() {
        return num;
    }
    if decimal_places_clob(num) <= decimals {
        return num;
    }
    let p = 10_f64.powi(decimals as i32);
    ((num + f64::EPSILON) * p).round() / p
}

fn round_down_clob(num: f64, decimals: u32) -> f64 {
    if !num.is_finite() {
        return num;
    }
    if decimal_places_clob(num) <= decimals {
        return num;
    }
    let p = 10_f64.powi(decimals as i32);
    (num * p).floor() / p
}

fn round_up_clob(num: f64, decimals: u32) -> f64 {
    if !num.is_finite() {
        return num;
    }
    if decimal_places_clob(num) <= decimals {
        return num;
    }
    let p = 10_f64.powi(decimals as i32);
    (num * p).ceil() / p
}

/// `@polymarket/clob-client` `getOrderRawAmounts` (`order-builder/helpers.ts`) — limit order
/// human `maker` / `taker` amounts before `parseUnits(..., 6)`.
fn get_order_raw_amounts_official(
    side: Side,
    size: f64,
    price: f64,
    price_dec: u32,
    size_dec: u32,
    amount_dec: u32,
) -> Option<(f64, f64)> {
    if !size.is_finite() || size <= 0.0 || !price.is_finite() {
        return None;
    }
    let raw_price = round_normal_clob(price, price_dec);
    if raw_price <= 0.0 {
        return None;
    }
    match side {
        Side::Buy => {
            let raw_taker = round_down_clob(size, size_dec);
            if raw_taker <= 0.0 {
                return None;
            }
            let mut raw_maker = raw_taker * raw_price;
            if decimal_places_clob(raw_maker) > amount_dec {
                raw_maker = round_up_clob(raw_maker, amount_dec.saturating_add(4));
                if decimal_places_clob(raw_maker) > amount_dec {
                    raw_maker = round_down_clob(raw_maker, amount_dec);
                }
            }
            if raw_maker <= 0.0 {
                return None;
            }
            Some((raw_maker, raw_taker))
        }
        Side::Sell => {
            let raw_maker = round_down_clob(size, size_dec);
            if raw_maker <= 0.0 {
                return None;
            }
            let mut raw_taker = raw_maker * raw_price;
            if decimal_places_clob(raw_taker) > amount_dec {
                raw_taker = round_up_clob(raw_taker, amount_dec.saturating_add(4));
                if decimal_places_clob(raw_taker) > amount_dec {
                    raw_taker = round_down_clob(raw_taker, amount_dec);
                }
            }
            if raw_taker <= 0.0 {
                return None;
            }
            Some((raw_maker, raw_taker))
        }
    }
}

fn round_normal_f64(num: f64, decimals: u32) -> f64 {
    let p = 10_f64.powi(decimals as i32);
    (num * p).round() / p
}

/// `py-clob-client` `to_token_decimals`: `int(round(1e6 * x))` for EIP-712 `maker`/`taker` amounts.
fn clob_float_to_micros_py_style(human: f64) -> U256 {
    if !human.is_finite() || human <= 0.0 {
        return U256::ZERO;
    }
    let f = human * 1_000_000.0;
    let f = round_normal_f64(f, 0);
    if f <= 0.0 {
        return U256::ZERO;
    }
    U256::from(f as u128)
}

fn round_down_f64(num: f64, decimals: u32) -> f64 {
    let p = 10_f64.powi(decimals as i32);
    (num * p).floor() / p
}

fn round_up_f64(num: f64, decimals: u32) -> f64 {
    let p = 10_f64.powi(decimals as i32);
    (num * p).ceil() / p
}

/// USDC on the wire: **exactly 2** fractional digits → micros are multiples of `10_000`.
fn usdc_human_to_wire_micros(human: f64) -> U256 {
    if !human.is_finite() || human <= 0.0 {
        return U256::ZERO;
    }
    let cents = (human * 100.0).round() as i64;
    let cents = cents.max(0) as u128;
    U256::from(cents * 10_000u128)
}

/// Outcome-token **shares** on the wire: **exactly 4** fractional digits → micros multiples of `100`.
fn share_human_to_wire_micros(human: f64) -> U256 {
    if !human.is_finite() || human <= 0.0 {
        return U256::ZERO;
    }
    let ten_thousandths = (human * 10_000.0).round() as i64;
    let ten_thousandths = ten_thousandths.max(0) as u128;
    U256::from(ten_thousandths * 100u128)
}

/// price×size → (makerAmount, takerAmount) in **1e6** base units (`parseUnits(..., 6)` in TS).
///
/// **GTC/GTD:** [`get_order_raw_amounts_official`] matches `@polymarket/clob-client` `getOrderRawAmounts`
/// then [`clob_float_to_micros_py_style`] on each leg (same as `parseUnits` + 6 decimals).
/// **`price`** must already be tick-snapped in [`TradingClient::place_order`].
///
/// **FAK/FOK:** book-aggressive price rounding; USDC legs use [`usdc_human_to_wire_micros`].
fn amounts_for(
    side: Side,
    size_shares: f64,
    price: f64,
    tick: &str,
    buy_notional_usdc: Option<f64>,
    order_type: OrderType,
) -> (U256, U256) {
    const USDC_DECIMALS: u32 = 2;
    const SHARE_DECIMALS_MAX: u32 = 4;

    let (price_dec, size_dec, amount_dec) = round_cfg_from_tick(tick);
    let share_dec = size_dec.min(SHARE_DECIMALS_MAX);
    let is_limit = matches!(order_type, OrderType::Gtc | OrderType::Gtd);

    if is_limit {
        return match side {
            Side::Buy => {
                let floor_notional = buy_notional_usdc
                    .filter(|n| n.is_finite() && *n > 0.0)
                    .map(|n| round_down_f64(n, USDC_DECIMALS))
                    .unwrap_or_else(|| {
                        let rp = round_normal_clob(price, price_dec);
                        round_up_f64(size_shares * rp, USDC_DECIMALS)
                    });

                if !size_shares.is_finite()
                    || size_shares <= 0.0
                    || !floor_notional.is_finite()
                    || floor_notional <= 0.0
                {
                    return (U256::ZERO, U256::ZERO);
                }

                let scale = 10_i64.pow(share_dec.min(6) as u32);
                let mut taker_ticks = ((size_shares * scale as f64).ceil() as i64).max(1);
                for _ in 0..50_000u32 {
                    let s = taker_ticks as f64 / scale as f64;
                    if let Some((raw_maker, raw_taker)) =
                        get_order_raw_amounts_official(Side::Buy, s, price, price_dec, size_dec, amount_dec)
                    {
                        if raw_maker + 1e-12 >= floor_notional {
                            return (
                                clob_float_to_micros_py_style(raw_maker),
                                clob_float_to_micros_py_style(raw_taker),
                            );
                        }
                    }
                    taker_ticks += 1;
                }
                (U256::ZERO, U256::ZERO)
            }
            Side::Sell => {
                let Some((raw_maker, raw_taker)) = get_order_raw_amounts_official(
                    Side::Sell,
                    size_shares,
                    price,
                    price_dec,
                    size_dec,
                    amount_dec,
                ) else {
                    return (U256::ZERO, U256::ZERO);
                };
                (
                    clob_float_to_micros_py_style(raw_maker),
                    clob_float_to_micros_py_style(raw_taker),
                )
            }
        };
    }

    let (raw_price, _price_micros) = {
        let rp = match side {
            Side::Buy => round_up_f64(price, price_dec),
            Side::Sell => round_down_f64(price, price_dec),
        };
        let pm = ((rp * 1_000_000.0).round() as i64).clamp(0, i64::MAX) as u64;
        (rp, pm)
    };

    match side {
        Side::Buy => {
            // Rounding shares down then USDC down can wipe a cent (e.g. 1 USDC → $0.99 vs $1 min).
            let floor_notional = buy_notional_usdc
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|n| round_down_f64(n, USDC_DECIMALS))
                .unwrap_or_else(|| round_up_f64(size_shares * raw_price, USDC_DECIMALS));

            if !size_shares.is_finite() || size_shares <= 0.0 || !floor_notional.is_finite() || floor_notional <= 0.0
            {
                return (U256::ZERO, U256::ZERO);
            }

            // Integer share ticks so we never get stuck on float `1.92 + 0.01`.
            let scale = 10_i64.pow(share_dec.min(6) as u32);
            let mut taker_ticks = ((size_shares * scale as f64).ceil() as i64).max(1);
            let mut raw_maker = 0.0_f64;
            let mut raw_taker = 0.0_f64;
            let mut guard = 0u32;
            while guard < 50_000 {
                raw_taker = taker_ticks as f64 / scale as f64;
                // `round_down(maker)` made implied price **below** the limit (e.g. $1.00/1.93 <
                // $0.52 ask) → FAK: "no orders found to match". `round_up` keeps USDC/shares ≥ limit.
                raw_maker = round_up_f64(raw_taker * raw_price, USDC_DECIMALS);
                if raw_maker + 1e-9 >= floor_notional {
                    break;
                }
                taker_ticks += 1;
                guard += 1;
            }
            (
                usdc_human_to_wire_micros(raw_maker),
                share_human_to_wire_micros(raw_taker),
            )
        }
        Side::Sell => {
            let raw_maker = round_down_f64(size_shares, share_dec);
            if raw_maker <= 0.0 || !raw_maker.is_finite() {
                return (U256::ZERO, U256::ZERO);
            }
            let product = raw_maker * raw_price;
            if product <= 0.0 || !product.is_finite() {
                return (U256::ZERO, U256::ZERO);
            }
            // `round_down` to cents can wipe sub-cent notionals (e.g. 0.5 shares × $0.01 → $0.005 → $0.00).
            // CLOB rejects with "maker and taker amounts must be higher than 0".
            let mut raw_taker = round_down_f64(product, USDC_DECIMALS);
            if raw_taker <= 0.0 {
                raw_taker = round_up_f64(product, USDC_DECIMALS);
            }
            // `usdc_human_to_wire_micros` maps to whole cents; e.g. $0.004 rounds to 0¢.
            let cents_wire = (raw_taker * 100.0).round() as i64;
            if cents_wire == 0 && raw_taker > 0.0 {
                raw_taker = 0.01;
            }
            (
                share_human_to_wire_micros(raw_maker),
                usdc_human_to_wire_micros(raw_taker),
            )
        }
    }
}

/// L2 HMAC-SHA256 signature. Matches Polymarket's reference:
/// `base64url( HMAC_SHA256(base64_decode(secret), ts+method+path+body) )`.
fn l2_hmac(secret_b64: &str, ts: i64, method: &str, path: &str, body: &str) -> Result<String> {
    // Polymarket's secrets are base64 url-safe, sometimes without padding.
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(secret_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(secret_b64))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(secret_b64))
        .context("decoding L2 secret")?;
    let mut mac = <Hmac<Sha256>>::new_from_slice(&key).map_err(|e| anyhow!("hmac key: {e}"))?;
    mac.update(ts.to_string().as_bytes());
    mac.update(method.as_bytes());
    mac.update(path.as_bytes());
    mac.update(body.as_bytes());
    let sig = mac.finalize().into_bytes();
    Ok(base64::engine::general_purpose::URL_SAFE.encode(sig))
}

/// Intermediate values produced during L1 auth signing — useful for diagnostics.
#[derive(Debug, Clone)]
pub struct ClobAuthDebug {
    pub address:     String,
    pub timestamp:   i64,
    pub nonce:       u64,
    pub type_hash:   String,
    pub domain_sep:  String,
    pub struct_hash: String,
    pub digest:      String,
    pub signature:   String,
}

/// User-facing message from CLOB JSON (`error` or `errorMsg`). Avoids putting full response bodies in the TUI.
fn extract_clob_error_text(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let s = v
        .get("error")
        .or_else(|| v.get("errorMsg"))
        .and_then(|x| x.as_str())?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Shorten long response bodies for log output.
fn snip(s: &str) -> String {
    let one_line: String = s.chars().filter(|&c| c != '\n' && c != '\r').collect();
    if one_line.chars().count() <= 200 { one_line }
    else { one_line.chars().take(200).collect::<String>() + "…" }
}

#[cfg(test)]
mod clob_error_text_tests {
    use super::extract_clob_error_text;

    #[test]
    fn parses_error_field() {
        assert_eq!(
            extract_clob_error_text(r#"{"error":"insufficient balance"}"#),
            Some("insufficient balance".into())
        );
    }

    #[test]
    fn parses_error_msg_alias() {
        assert_eq!(
            extract_clob_error_text(r#"{"errorMsg":"invalid price"}"#),
            Some("invalid price".into())
        );
    }

    #[test]
    fn non_json_yields_none() {
        assert_eq!(extract_clob_error_text("not json"), None);
    }
}

#[cfg(test)]
mod take_profit_fill_tests {
    use super::PostOrderResponse;

    fn r(
        success: bool,
        status: Option<&str>,
        making: Option<&str>,
        taking: Option<&str>,
    ) -> PostOrderResponse {
        PostOrderResponse {
            success,
            order_id: None,
            status: status.map(String::from),
            making_amount: making.map(String::from),
            taking_amount: taking.map(String::from),
            error: None,
        }
    }

    #[test]
    fn delayed_without_amounts_uses_request() {
        let p = r(true, Some("delayed"), None, None);
        let o = p.take_profit_fill_for_market_buy(10.0, 0.5).unwrap();
        assert!((o.0 - 10.0).abs() < 1e-9);
        assert!((o.1 - 0.5).abs() < 1e-9);
        assert!(!o.2);
    }

    #[test]
    fn amounts_from_api_preferred() {
        let p = r(true, Some("matched"), Some("2"), Some("4"));
        let o = p.take_profit_fill_for_market_buy(99.0, 0.99).unwrap();
        assert!((o.0 - 4.0).abs() < 1e-9);
        assert!((o.1 - 0.5).abs() < 1e-9);
        assert!(o.2);
    }

    /// CLOB returns decimal USDC / shares, not 1e6-scaled integers.
    #[test]
    fn matched_buy_parses_decimal_strings_from_clob() {
        let p = r(true, Some("matched"), Some("1.009999"), Some("1.463767"));
        let (sh, px) = p.matched_buy_fill_shares_and_avg_price().unwrap();
        assert!((sh - 1.463767).abs() < 1e-6);
        assert!((px - 1.009999 / 1.463767).abs() < 1e-6);
    }

    #[test]
    fn live_without_amounts_is_none() {
        let p = r(true, Some("live"), None, None);
        assert!(p.take_profit_fill_for_market_buy(10.0, 0.5).is_none());
    }

    /// Implausible dust share count vs a normal ticket → fall back to the submitted order.
    #[test]
    fn implausible_api_dust_falls_back_to_request() {
        let p = r(true, Some("matched"), Some("1.0"), Some("0.0000001"));
        let o = p.take_profit_fill_for_market_buy(10.0, 0.55).unwrap();
        assert!((o.0 - 10.0).abs() < 1e-9);
        assert!((o.1 - 0.55).abs() < 1e-9);
        assert!(!o.2);
    }
}

#[cfg(test)]
mod fill_for_position_ack_tests {
    use super::{OrderType, PostOrderResponse, Side};

    fn r(
        success: bool,
        status: Option<&str>,
        making: Option<&str>,
        taking: Option<&str>,
    ) -> PostOrderResponse {
        PostOrderResponse {
            success,
            order_id: None,
            status: status.map(String::from),
            making_amount: making.map(String::from),
            taking_amount: taking.map(String::from),
            error: None,
        }
    }

    #[test]
    fn resting_limit_does_not_fake_position() {
        let p = r(true, Some("live"), None, None);
        assert!(p.fill_for_position_ack(Side::Buy, 10.0, 0.5, OrderType::Gtd).is_none());
        let p = r(true, Some("open"), None, None);
        assert!(p.fill_for_position_ack(Side::Sell, 10.0, 0.5, OrderType::Gtd).is_none());
    }

    #[test]
    fn matched_without_amounts_uses_submitted_size() {
        let p = r(true, Some("matched"), None, None);
        let (q, px) = p.fill_for_position_ack(Side::Buy, 12.0, 0.48, OrderType::Gtd).unwrap();
        assert!((q - 12.0).abs() < 1e-9);
        assert!((px - 0.48).abs() < 1e-9);
    }

    #[test]
    fn fak_success_without_status_still_updates_position() {
        let p = PostOrderResponse {
            success: true,
            order_id: None,
            status: None,
            making_amount: None,
            taking_amount: None,
            error: None,
        };
        let (q, px) = p.fill_for_position_ack(Side::Buy, 4.0, 0.6, OrderType::Fak).unwrap();
        assert!((q - 4.0).abs() < 1e-9);
        assert!((px - 0.6).abs() < 1e-9);
    }

    #[test]
    fn partial_fill_amounts_preferred_for_sell() {
        let p = r(true, Some("live"), Some("2.0"), Some("0.9"));
        let (q, px) = p.fill_for_position_ack(Side::Sell, 10.0, 0.5, OrderType::Gtd).unwrap();
        assert!((q - 2.0).abs() < 1e-9);
        assert!((px - 0.45).abs() < 1e-9);
    }

    /// Small but real `takingAmount` can fail `parsed_buy_fill_plausible` vs a large request; FAK path
    /// still uses the exchange-reported shares.
    #[test]
    fn fak_buy_prefers_api_over_request_when_plausible_rejects_small_fill() {
        let p = r(true, Some("matched"), Some("0.00025"), Some("0.0005"));
        let strict = p.fill_for_position_ack(Side::Buy, 10.0, 0.5, OrderType::Fak).unwrap();
        assert!((strict.0 - 10.0).abs() < 1e-9);
        let loose = p.fak_fill_for_position_ack(Side::Buy, 10.0, 0.5).unwrap();
        assert!((loose.0 - 0.0005).abs() < 1e-12);
        assert!((loose.1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn fak_buy_uses_exact_taking_amount_vs_rough_request() {
        let p = r(
            true,
            Some("matched"),
            Some("5.04435025"),
            Some("10.087"),
        );
        let (q, px) = p.fak_fill_for_position_ack(Side::Buy, 10.0, 0.5).unwrap();
        assert!((q - 10.087).abs() < 1e-9);
        assert!((px - 5.04435025 / 10.087).abs() < 1e-9);
    }

    #[test]
    fn fak_sell_uses_exact_making_amount() {
        let p = r(true, Some("matched"), Some("10.087"), Some("5.04"));
        let (q, px) = p.fak_fill_for_position_ack(Side::Sell, 10.0, 0.5).unwrap();
        assert!((q - 10.087).abs() < 1e-9);
        assert!((px - 5.04 / 10.087).abs() < 1e-9);
    }
}

#[cfg(test)]
mod take_profit_reconcile_tests {
    use super::{net_shares_on_token_from_trades, take_profit_sell_cap, ClobTrade};

    fn trade(id: &str, asset: &str, side: &str, size: &str) -> ClobTrade {
        ClobTrade {
            id: id.to_string(),
            asset_id: asset.to_string(),
            side: side.to_string(),
            size: size.to_string(),
            price: "0.5".to_string(),
            match_time: "0".to_string(),
            taker_order_id: None,
            maker_orders: vec![],
            trader_side: None,
        }
    }

    #[test]
    fn net_buy_minus_sell_on_token() {
        let t = vec![
            trade("1", "tok", "BUY", "10"),
            trade("2", "tok", "SELL", "3"),
        ];
        assert!((net_shares_on_token_from_trades(&t, "tok") - 7.0).abs() < 1e-9);
    }

    /// Observed in the wild: `takingAmount` a hair above conditional balance / trade replay.
    #[test]
    fn cap_matches_conservative_min_when_api_round_high() {
        let c = take_profit_sell_cap(5.329783, 5.306763, 5.306763);
        assert!((c - 5.306763).abs() < 1e-5);
    }

    #[test]
    fn cap_prefers_replay_when_balance_not_yet_updated() {
        let c = take_profit_sell_cap(5.0, 0.0, 5.2);
        assert!((c - 5.0).abs() < 1e-9);
    }

    #[test]
    fn cap_prefers_balance_when_trade_list_lags() {
        let c = take_profit_sell_cap(5.0, 5.2, 4.0);
        assert!((c - 5.0).abs() < 1e-9);
    }

    /// Replay / fills above wallet (fees, rounding) must not size above balance.
    #[test]
    fn cap_when_replay_high_uses_min_of_need_balance_and_replay() {
        let c = take_profit_sell_cap(9.82, 9.476958, 9.82);
        assert!((c - 9.476958).abs() < 1e-5);
    }
}

#[cfg(test)]
mod buy_fill_order_id_tests {
    use super::{
        buy_fill_shares_from_trades_for_order, order_ids_match, ClobMakerOrder, ClobTrade,
    };

    #[test]
    fn order_ids_ignore_0x_and_case() {
        assert!(order_ids_match("0xAbC", "abc"));
        assert!(order_ids_match("AbC", "0xabc"));
        assert!(!order_ids_match("a", "b"));
    }

    #[test]
    fn taker_buy_fill_by_taker_order_id() {
        let oid = "0xdeadbeef";
        let trades = vec![ClobTrade {
            id: "t1".into(),
            asset_id: "tok1".into(),
            side: "BUY".into(),
            size: "5.25".into(),
            price: "0.5".into(),
            match_time: "0".into(),
            taker_order_id: Some(oid.to_string()),
            maker_orders: vec![],
            trader_side: Some("TAKER".into()),
        }];
        let f = buy_fill_shares_from_trades_for_order(&trades, oid, "tok1").unwrap();
        assert!((f - 5.25).abs() < 1e-9);
    }

    #[test]
    fn maker_buy_fill_uses_matched_amount() {
        let oid = "order-maker-1";
        let trades = vec![ClobTrade {
            id: "t1".into(),
            asset_id: "tok1".into(),
            side: "BUY".into(),
            size: "99".into(),
            price: "0.5".into(),
            match_time: "0".into(),
            taker_order_id: None,
            maker_orders: vec![ClobMakerOrder {
                order_id: oid.to_string(),
                matched_amount: "3.5".into(),
            }],
            trader_side: Some("MAKER".into()),
        }];
        let f = buy_fill_shares_from_trades_for_order(&trades, oid, "tok1").unwrap();
        assert!((f - 3.5).abs() < 1e-9);
    }

    #[test]
    fn no_matching_order_returns_none() {
        let trades = vec![ClobTrade {
            id: "t1".into(),
            asset_id: "tok1".into(),
            side: "BUY".into(),
            size: "5".into(),
            price: "0.5".into(),
            match_time: "0".into(),
            taker_order_id: Some("other".into()),
            maker_orders: vec![],
            trader_side: Some("TAKER".into()),
        }];
        assert!(buy_fill_shares_from_trades_for_order(&trades, "want-this", "tok1").is_none());
    }
}

#[cfg(test)]
mod limit_price_snap_tests {
    use super::{snap_limit_order_price_to_tick, snap_limit_price_micros};

    fn tick_micros(tick: &str) -> u64 {
        let t = tick.trim().parse::<f64>().expect("tick parse");
        (t * 1_000_000.0).round() as u64
    }

    #[test]
    fn gtd_float_snaps_to_min_tick_decimals() {
        let p = snap_limit_order_price_to_tick(0.7302551640340219, "0.01");
        assert!((p - 0.73).abs() < 1e-9);
    }

    /// Regression: float tails near 0.99 must not produce off-tick `price_micros` (CLOB tick error).
    #[test]
    fn gtd_price_micros_exact_multiple_of_0_01_tick() {
        let m = snap_limit_price_micros(0.9901153212520593, "0.01");
        assert_eq!(m, 990_000);
        assert_eq!(m % 10_000, 0);
    }

    #[test]
    fn snap_clamps_below_min_to_one_cent() {
        assert_eq!(snap_limit_price_micros(0.005, "0.01"), 10_000);
        assert!((snap_limit_order_price_to_tick(0.005, "0.01") - 0.01).abs() < 1e-12);
    }

    #[test]
    fn snap_clamps_above_max_to_99_cents() {
        assert_eq!(snap_limit_price_micros(1.5, "0.01"), 990_000);
        assert!((snap_limit_order_price_to_tick(2.0, "0.01") - 0.99).abs() < 1e-12);
    }

    #[test]
    fn snap_non_finite_price_falls_back_to_one_cent() {
        assert_eq!(snap_limit_price_micros(f64::NAN, "0.01"), 10_000);
        assert_eq!(snap_limit_price_micros(f64::INFINITY, "0.01"), 10_000);
        assert_eq!(snap_limit_price_micros(f64::NEG_INFINITY, "0.01"), 10_000);
    }

    #[test]
    fn snap_trims_tick_whitespace() {
        assert_eq!(snap_limit_price_micros(0.44, "  0.01  "), 440_000);
    }

    #[test]
    fn snap_nearest_on_0_01_grid() {
        let tm = tick_micros("0.01");
        for (human, want_micros) in [
            (0.514999, 510_000),
            (0.515, 520_000),
            (0.01, 10_000),
            (0.99, 990_000),
        ] {
            let m = snap_limit_price_micros(human, "0.01");
            assert_eq!(m, want_micros, "human={human}");
            assert_eq!(m % tm, 0);
        }
    }

    #[test]
    fn snap_nearest_on_0_001_grid() {
        let tm = tick_micros("0.001");
        let m = snap_limit_price_micros(0.456789, "0.001");
        assert_eq!(m, 457_000);
        assert_eq!(m % tm, 0);
    }

    #[test]
    fn snap_nearest_on_0_1_grid() {
        let tm = tick_micros("0.1");
        let m = snap_limit_price_micros(0.35, "0.1");
        assert_eq!(m, 400_000);
        assert_eq!(m % tm, 0);
    }

    #[test]
    fn snap_order_price_matches_micros_over_1e6() {
        for human in [0.12_f64, 0.55, 0.73, 0.99] {
            let m = snap_limit_price_micros(human, "0.01");
            let p = snap_limit_order_price_to_tick(human, "0.01");
            assert!((p * 1_000_000.0).round() as u64 >= 10_000);
            assert_eq!((p * 1_000_000.0).round() as u64, m, "human={human}");
        }
    }
}

#[cfg(test)]
mod limit_gtd_sell_amounts_tests {
    use super::{amounts_for, OrderType, Side};
    use alloy_primitives::U256;

    fn implied_price_per_share(maker_micros: U256, taker_micros: U256) -> f64 {
        let m: u128 = maker_micros.try_into().expect("maker fits u128");
        let t: u128 = taker_micros.try_into().expect("taker fits u128");
        if m == 0 {
            return 0.0;
        }
        t as f64 / m as f64
    }

    /// SELL: maker = shares, taker = USDC → implied USDC/share = taker/maker.
    fn sell_implied_price_micros(maker: U256, taker: U256) -> u128 {
        let m: u128 = maker.try_into().expect("maker");
        let t: u128 = taker.try_into().expect("taker");
        assert!(m > 0);
        t.saturating_mul(1_000_000) / m
    }

    #[test]
    fn gtd_sell_implied_price_on_tick_despite_input_float_tail() {
        let noisy = 0.9804305283757339;
        let (m1, t1) = amounts_for(Side::Sell, 9.82, noisy, "0.01", None, OrderType::Gtd);
        let (m2, t2) = amounts_for(Side::Sell, 9.82, 0.98, "0.01", None, OrderType::Gtd);
        assert_eq!(m1, m2);
        assert_eq!(t1, t2);
        let px = implied_price_per_share(m1, t1);
        assert!((px - 0.98).abs() < 1e-14, "implied={}", px);
    }

    #[test]
    fn gtd_sell_whole_shares_implied_exactly_on_tick() {
        let (mk, tk) = amounts_for(Side::Sell, 10.0, 0.51, "0.01", None, OrderType::Gtd);
        assert!(!mk.is_zero() && !tk.is_zero());
        let implied_micros = sell_implied_price_micros(mk, tk);
        assert_eq!(implied_micros, 510_000);
        assert_eq!(implied_micros % 10_000, 0);
    }

    #[test]
    fn gtd_sell_tick_0_001_implied_aligns_to_tick() {
        let (mk, tk) = amounts_for(Side::Sell, 5.0, 0.1237, "0.001", None, OrderType::Gtd);
        assert!(!mk.is_zero() && !tk.is_zero());
        let implied_micros = sell_implied_price_micros(mk, tk);
        assert_eq!(implied_micros % 1_000, 0);
    }
}

#[cfg(test)]
mod limit_gtd_buy_amounts_tests {
    use super::{amounts_for, OrderType, Side};
    use alloy_primitives::U256;

    /// BUY limit: maker = USDC, taker = outcome shares → implied = maker/taker.
    fn buy_implied_price_micros(maker: U256, taker: U256) -> u128 {
        let m: u128 = maker.try_into().expect("maker");
        let t: u128 = taker.try_into().expect("taker");
        assert!(t > 0);
        m.saturating_mul(1_000_000) / t
    }

    #[test]
    fn gtd_buy_with_notional_implied_on_0_01_tick() {
        let (mk, tk) = amounts_for(
            Side::Buy,
            250.0,
            0.55,
            "0.01",
            Some(100.0),
            OrderType::Gtd,
        );
        assert!(!mk.is_zero() && !tk.is_zero());
        let implied = buy_implied_price_micros(mk, tk);
        assert_eq!(implied % 10_000, 0, "implied_micros={implied}");
    }

    #[test]
    fn gtd_buy_one_dollar_notional_implied_on_tick() {
        let (mk, tk) = amounts_for(
            Side::Buy,
            50.0,
            0.73,
            "0.01",
            Some(1.0),
            OrderType::Gtd,
        );
        assert!(!mk.is_zero() && !tk.is_zero());
        assert_eq!(buy_implied_price_micros(mk, tk) % 10_000, 0);
    }

    #[test]
    fn gtd_buy_no_notional_uses_size_times_price_budget() {
        let (mk, tk) = amounts_for(Side::Buy, 20.0, 0.40, "0.01", None, OrderType::Gtd);
        assert!(!mk.is_zero() && !tk.is_zero());
        assert_eq!(buy_implied_price_micros(mk, tk) % 10_000, 0);
    }
}

#[cfg(test)]
mod fak_market_sell_amounts_tests {
    use super::{amounts_for, OrderType, Side};
    use alloy_primitives::U256;

    /// Sub-cent `price × shares` used to yield `takerAmount=0` on the wire (CLOB: amounts must be > 0).
    #[test]
    fn fak_sell_half_share_at_one_cent_rounds_taker_to_one_cent() {
        let (mk, tk) = amounts_for(Side::Sell, 0.5, 0.01, "0.01", None, OrderType::Fak);
        assert!(!mk.is_zero() && !tk.is_zero());
        assert_eq!(tk, U256::from(10_000u64), "taker should be $0.01 in 1e6 micros");
    }

    #[test]
    fn fak_sell_four_mils_usdc_rounds_to_one_cent() {
        // 0.04 × $0.10 = $0.004 → 0¢ if rounded naively; must still be non-zero.
        let (mk, tk) = amounts_for(Side::Sell, 0.04, 0.10, "0.01", None, OrderType::Fak);
        assert!(!mk.is_zero() && !tk.is_zero());
        assert_eq!(tk, U256::from(10_000u64));
    }
}