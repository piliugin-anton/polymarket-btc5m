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
use std::str::FromStr;
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
    #[serde(default)] #[serde(rename = "orderID")]
    #[allow(dead_code)]
    pub order_id: Option<String>,
    #[serde(default)] pub status:   Option<String>,
    /// USDC leg for BUY, shares leg for SELL — 1e6 fixed decimals (see Polymarket `SendOrderResponse`).
    #[serde(default, rename = "makingAmount")]
    pub making_amount: Option<String>,
    /// Shares leg for BUY, USDC leg for SELL — 1e6 fixed decimals.
    #[serde(default, rename = "takingAmount")]
    pub taking_amount: Option<String>,
    #[serde(default, rename = "errorMsg")]
    pub error:    Option<String>,
}

impl PostOrderResponse {
    /// After a matched **BUY**: filled shares and average price from `makingAmount` / `takingAmount`.
    pub fn matched_buy_fill_shares_and_avg_price(&self) -> Option<(f64, f64)> {
        let mk = self.making_amount.as_ref()?.trim();
        let tk = self.taking_amount.as_ref()?.trim();
        if mk.is_empty() || tk.is_empty() {
            return None;
        }
        let maker_usdc = mk.parse::<f64>().ok()? / 1_000_000.0;
        let taker_sh = tk.parse::<f64>().ok()? / 1_000_000.0;
        if taker_sh <= 1e-12 || !maker_usdc.is_finite() {
            return None;
        }
        Some((taker_sh, maker_usdc / taker_sh))
    }
}

/// `GET /fee-rate` — <https://docs.polymarket.com/api-reference/market-data/get-fee-rate>
#[derive(Debug, Deserialize)]
struct FeeRateResponse {
    #[serde(rename = "base_fee")]
    base_fee: u64,
}

/// Polymarket `clob-client-v2` `GET /data/trades` — one row per user fill (L2 auth).
/// Shape matches `Trade` in `clob-client-v2` `types/clob.ts`.
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

// ── Client ──────────────────────────────────────────────────────────

pub struct TradingClient {
    http:    reqwest::Client,
    signer:  PrivateKeySigner,
    creds:   Option<ApiCreds>,
    config:  Config,
    /// Cached `GET /version` → `version` field (1 or 2). Polymarket `clob-client-v2` uses this
    /// to pick EIP-712 domain + struct; production Polygon currently returns **1**.
    cached_clob_order_version: Option<u32>,
}

impl TradingClient {
    pub fn new(config: Config) -> Result<Self> {
        let signer: PrivateKeySigner = config.private_key.parse()
            .context("parsing private key")?;
        Ok(Self {
            http: crate::net::reqwest_client()?,
            signer,
            creds: None,
            config,
            cached_clob_order_version: None,
        })
    }

    async fn fetch_clob_order_version(&mut self) -> Result<u32> {
        if let Some(v) = self.cached_clob_order_version {
            return Ok(v);
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
        self.cached_clob_order_version = Some(version);
        Ok(version)
    }

    /// Derive (or create) the L2 API credentials by producing an EIP-712
    /// signature over the `ClobAuth` struct. Tries GET /auth/derive-api-key
    /// first and falls back to POST /auth/api-key if no keys exist yet.
    pub async fn ensure_creds(&mut self) -> Result<ApiCreds> {
        if let Some(c) = &self.creds { return Ok(c.clone()); }

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
            self.creds = Some(creds.clone());
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
            self.creds = Some(creds.clone());
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
            "CLOB auth failed. GET /auth/derive-api-key → {} \"{}\". POST /auth/api-key → {} \"{}\". Run `btc5m-bot debug-auth` for a full dump.",
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
    /// `btc5m-bot debug-auth`.
    pub async fn debug_auth_flow(&mut self) -> Result<()> {
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
            "fetch_fee_rate_bps"
        );
        Ok(fr.base_fee)
    }

    /// `GET /balance-allowance/update` — asks CLOB to refresh its balance cache before a read.
    /// Matches official `clob-client` `updateBalanceAllowance`; mitigates stale `0` balances
    /// ([clob-client#128](https://github.com/Polymarket/clob-client/issues/128)).
    async fn update_conditional_balance_allowance(&mut self, token_id: &str) -> Result<()> {
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

    /// Conditional (outcome token) balance for `token_id`, in **human shares** (raw / 1e6).
    ///
    /// Uses L2 auth — same as `clob-client-v2` `getBalanceAllowance` with
    /// `asset_type: CONDITIONAL`.
    pub async fn fetch_conditional_balance_shares(&mut self, token_id: &str) -> Result<f64> {
        let _ = self.update_conditional_balance_allowance(token_id).await;
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

    /// USDC **collateral** balance + spending allowance (`GET /balance-allowance`, `asset_type=COLLATERAL`).
    ///
    /// Raw amounts use **6 decimals** (same as conditional shares in this client). The allowance
    /// is the ERC-20 approval the funder granted to Polymarket contracts — often near-unlimited.
    pub async fn fetch_collateral_cash_and_cashout_usdc(&mut self) -> Result<(f64, f64)> {
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

    /// All trades for `condition_id` (Gamma `conditionId` / CLOB `market`), paginated like TS `getTrades`.
    pub async fn fetch_trades_for_market(&mut self, condition_id: &str) -> Result<Vec<ClobTrade>> {
        let creds = self.ensure_creds().await?;
        let path = "/data/trades";
        let mut cursor = TRADES_INITIAL_CURSOR.to_string();
        let mut out = Vec::new();
        loop {
            let ts = chrono::Utc::now().timestamp();
            let mut url = url::Url::parse(&format!("{CLOB_HOST}{path}"))
                .context("parse /data/trades URL")?;
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
    pub async fn fetch_open_orders_for_market(&mut self, condition_id: &str) -> Result<Vec<ClobOpenOrder>> {
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
    pub async fn place_order(&mut self, args: OrderArgs, order_type: OrderType)
        -> Result<PostOrderResponse>
    {
        let creds = self.ensure_creds().await?;
        let fee_bps = self.fetch_fee_rate_bps(&args.token_id).await?;

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

        // 2. Salt — must fit in JS `Number` (see `orderToJsonV1` / `orderToJsonV2` `parseInt`).
        let ts_ms = chrono::Utc::now().timestamp_millis();
        let salt = (rand::random::<f64>() * ts_ms as f64).round() as u64;

        let api_version = self.fetch_clob_order_version().await?;
        if order_type == OrderType::Gtd && api_version != 1 {
            bail!(
                "GTD orders need CLOB signing API version 1 (EIP-712 includes expiration); server returned {api_version}"
            );
        }
        let token_id_u256 = U256::from_str(&args.token_id).context("token_id")?;

        let verifying_v1 = if args.neg_risk { NEG_RISK_CTF_EXCHANGE_V1 } else { CTF_EXCHANGE_V1 };
        let verifying_v2 = if args.neg_risk { NEG_RISK_CTF_EXCHANGE_V2 } else { CTF_EXCHANGE_V2 };

        const PUBLIC_TAKER: &str = "0x0000000000000000000000000000000000000000";
        const ZERO32: &str =
            "0x0000000000000000000000000000000000000000000000000000000000000000";

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

        self.post_order_http(&creds, body).await
    }

    async fn post_order_http(&mut self, creds: &ApiCreds, body: String) -> Result<PostOrderResponse> {
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
            return Err(anyhow!("CLOB POST /order failed: {} — {}", status, txt));
        }
        let out: PostOrderResponse = serde_json::from_str(&txt)
            .with_context(|| format!("decoding order response: {}", snip(&txt)))?;
        Ok(out)
    }

    /// Cancel all open orders for this user.
    pub async fn cancel_all(&mut self) -> Result<()> {
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
        if !resp.status().is_success() {
            return Err(anyhow!("cancel-all failed: {}", resp.status()));
        }
        Ok(())
    }
}

/// Polymarket `ROUNDING_CONFIG` (`roundingConfig.ts`): `(price_dec, size_dec)` for price / share rounding.
/// Nearest tick (e.g. 0.51269 + tick 0.01 → 0.51). Avoids float drift on `n * tick`.
fn snap_price_to_nearest_tick(price: f64, tick: &str) -> f64 {
    let tick_f = tick.trim().parse::<f64>().unwrap_or(0.01);
    let (price_dec, _) = round_cfg_from_tick(tick);
    if !price.is_finite() || tick_f <= 0.0 {
        return price;
    }
    let n = (price / tick_f).round();
    round_down_f64(n * tick_f, price_dec)
}

fn round_cfg_from_tick(tick: &str) -> (u32, u32) {
    let x = tick.trim().parse::<f64>().unwrap_or(0.01);
    if (x - 0.1).abs() < 1e-6 {
        (1, 2)
    } else if (x - 0.01).abs() < 1e-7 {
        (2, 2)
    } else if (x - 0.001).abs() < 1e-8 {
        (3, 2)
    } else if (x - 0.0001).abs() < 1e-9 {
        (4, 2)
    } else {
        (2, 2)
    }
}

fn round_down_f64(num: f64, decimals: u32) -> f64 {
    let p = 10_f64.powi(decimals as i32);
    (num * p).floor() / p
}

fn round_up_f64(num: f64, decimals: u32) -> f64 {
    let p = 10_f64.powi(decimals as i32);
    (num * p).ceil() / p
}

fn human_to_base_units_6(human: f64) -> U256 {
    if !human.is_finite() || human <= 0.0 {
        return U256::ZERO;
    }
    let micros = (human * 1_000_000.0).floor() as u128;
    U256::from(micros)
}

/// price×size → (makerAmount, takerAmount) in **1e6** base units (`parseUnits(..., 6)` in TS).
///
/// CLOB validates **market-style** orders as: **USDC leg ≤ 2** decimal places, **share leg ≤ 4**
/// (buy: maker=USDC / taker=shares; sell: maker=shares / taker=USDC). The TS `amount` field in
/// `ROUNDING_CONFIG` is *not* this limit — using it produced USDC with up to 6 decimals and broke buys.
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

    let (price_dec, size_dec) = round_cfg_from_tick(tick);
    let share_dec = size_dec.min(SHARE_DECIMALS_MAX);
    let is_limit = matches!(order_type, OrderType::Gtc | OrderType::Gtd);

    // GTC/GTD: snap to tick first so EIP-712 amounts imply a tick-valid price (CLOB rejects
    // e.g. 0.5126903553299492 vs tick 0.01). FAK/FOK: aggressive rounding vs the book.
    let raw_price = if is_limit {
        snap_price_to_nearest_tick(price, tick)
    } else {
        match side {
            Side::Buy => round_up_f64(price, price_dec),
            Side::Sell => round_down_f64(price, price_dec),
        }
    };
    let price_micros: u64 = ((raw_price * 1_000_000.0).round() as i64).clamp(0, i64::MAX) as u64;

    match side {
        // Limit BUY: USDC (maker) and shares (taker) from integer micros so implied price matches tick.
        Side::Buy if is_limit => {
            let floor_notional = buy_notional_usdc
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|n| round_down_f64(n, USDC_DECIMALS))
                .unwrap_or_else(|| round_up_f64(size_shares * raw_price, USDC_DECIMALS));

            if !size_shares.is_finite() || size_shares <= 0.0 || !floor_notional.is_finite() || floor_notional <= 0.0
            {
                return (U256::ZERO, U256::ZERO);
            }

            let scale = 10_i64.pow(share_dec.min(6) as u32);
            let mut taker_ticks = ((size_shares * scale as f64).ceil() as i64).max(1);
            let mut guard = 0u32;
            while guard < 50_000 {
                let raw_taker = taker_ticks as f64 / scale as f64;
                let tm = (raw_taker * 1_000_000.0).round() as u64;
                let mm = (tm as u128 * price_micros as u128 + 999_999) / 1_000_000;
                let raw_maker = mm as f64 / 1_000_000.0;
                if raw_maker + 1e-9 >= floor_notional {
                    return (human_to_base_units_6(raw_maker), human_to_base_units_6(raw_taker));
                }
                taker_ticks += 1;
                guard += 1;
            }
            (U256::ZERO, U256::ZERO)
        }
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
            (human_to_base_units_6(raw_maker), human_to_base_units_6(raw_taker))
        }
        Side::Sell if is_limit => {
            let raw_maker = round_down_f64(size_shares, share_dec);
            let maker_micros = (raw_maker * 1_000_000.0).round() as u64;
            let taker_micros = (maker_micros as u128 * price_micros as u128) / 1_000_000;
            let raw_taker = taker_micros as f64 / 1_000_000.0;
            (human_to_base_units_6(raw_maker), human_to_base_units_6(raw_taker))
        }
        Side::Sell => {
            let raw_maker = round_down_f64(size_shares, share_dec);
            let raw_taker = round_down_f64(raw_maker * raw_price, USDC_DECIMALS);
            (human_to_base_units_6(raw_maker), human_to_base_units_6(raw_taker))
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

/// Shorten long response bodies for log output.
fn snip(s: &str) -> String {
    let one_line: String = s.chars().filter(|&c| c != '\n' && c != '\r').collect();
    if one_line.chars().count() <= 200 { one_line }
    else { one_line.chars().take(200).collect::<String>() + "…" }
}