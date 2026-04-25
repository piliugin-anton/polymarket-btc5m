//! Configuration: endpoints, env vars, chain constants.
//!
//! All trading-sensitive values come from environment variables. See `.env.example`.

use anyhow::{bail, Context, Result};
use alloy_primitives::Address;
use std::str::FromStr;

// ── Polymarket endpoints ────────────────────────────────────────────
pub const CLOB_HOST: &str   = "https://clob.polymarket.com";
pub const GAMMA_HOST: &str  = "https://gamma-api.polymarket.com";
/// Official rolling-window crypto reference price (open / close) for 5m BTC markets.
pub const POLYMARKET_CRYPTO_PRICE_URL: &str = "https://polymarket.com/api/crypto/crypto-price";
pub const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
/// Authenticated user stream (`trade` / `order`). See Polymarket user channel docs.
pub const CLOB_WS_USER_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";
pub const RTDS_WS_URL: &str = "wss://ws-live-data.polymarket.com";

// ── Polygon ─────────────────────────────────────────────────────────
pub const POLYGON_CHAIN_ID: u64 = 137;

// CTF Exchange — V1 EIP-712 (`domain.version` = "1"). Used while `GET /version` returns 1.
// From Polymarket `clob-client-v2` `MATIC_CONTRACTS` (`exchange` / `negRiskExchange`).
pub const CTF_EXCHANGE_V1: &str          = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
pub const NEG_RISK_CTF_EXCHANGE_V1: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

// CTF Exchange — V2 EIP-712 (`domain.version` = "2") for when `GET /version` returns 2.
// https://docs.polymarket.com/v2-migration#for-api-users
pub const CTF_EXCHANGE_V2: &str          = "0xE111180000d2663C0091e4f400237545B87B996B";
pub const NEG_RISK_CTF_EXCHANGE_V2: &str = "0xe2222d279d744050d28e00520010520000310F59";

/// Signature type (see CTF Exchange `OrderStructs.sol`).
///
/// * 0 = `EOA` — you hold a plain wallet and trade directly (rare on Polymarket).
/// * 1 = `POLY_PROXY` — legacy magic/email-based proxy wallet.
/// * 2 = `POLY_GNOSIS_SAFE` — the current default: your EOA owns a Gnosis Safe that
///       holds the USDC. `funder` = safe address, `signer` = EOA address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    Eoa           = 0,
    PolyProxy     = 1,
    PolyGnosisSafe = 2,
}

/// Runtime configuration — resolved once at startup.
#[derive(Debug, Clone)]
pub struct Config {
    /// Private key (hex, 0x-prefixed) of the EOA that signs orders.
    pub private_key: String,
    /// Address that actually funds the trades. For Safe proxy: the Safe address;
    /// for EOA: same as the EOA.
    pub funder: Address,
    /// EOA address derived from `private_key` — filled by `signer.address()`.
    pub signer_address: Address,
    pub sig_type: SignatureType,
    /// Default USDC ticket for **market BUY** (`DEFAULT_SIZE_USDC`). Market SELL uses full in-app position when known.
    pub default_size_usdc: f64,
    /// Max fill slippage for market **buy** FAK orders (basis points; widens buy ceiling vs best ask).
    pub market_buy_slippage_bps: u32,
    /// Max fill slippage for market **sell** FAK orders (basis points; widens sell floor vs best bid).
    pub market_sell_slippage_bps: u32,
    /// Bps margin: (1) with trailing **off** — GTD SELL after a market **Buy** at this edge
    /// ([`crate::fees::take_profit_limit_price_crypto_after_fees`]). (2) with trailing **on** — trail
    /// **arms** when `best_bid >= entry * (1 + bps/10_000)` with `entry` from the live position
    /// (`avg_entry`) or the fill estimate until the position is applied (`0` = arm as soon as bid
    /// reaches entry).
    pub market_buy_take_profit_bps: u32,
    /// If positive after a market **Buy** (FAK), run a trailing stop on CLOB best bid, then FAK SELL; trail width in bps from peak.
    pub market_buy_trail_bps: u32,
    /// Polymarket Relayer API key (Settings → API) — required for gasless Safe `execTransaction` (CTF redeem).
    pub relayer_api_key: Option<String>,
    /// Address paired with the relayer API key (same screen in Polymarket settings).
    pub relayer_api_key_address: Option<Address>,
    /// Polygon JSON-RPC URL for `eth_call` (neg-risk balances). Public default if unset.
    pub polygon_rpc_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // .env is optional — ignore failure
        let _ = dotenvy::dotenv();

        let private_key = std::env::var("POLYMARKET_PK")
            .context("POLYMARKET_PK env var (0x-prefixed hex private key) is required")?;

        let funder_str = std::env::var("POLYMARKET_FUNDER")
            .context("POLYMARKET_FUNDER env var (funder/safe address) is required")?;
        let funder = Address::from_str(&funder_str)
            .context("POLYMARKET_FUNDER is not a valid 0x-prefixed address")?;

        let sig_type = match std::env::var("POLYMARKET_SIG_TYPE")
            .unwrap_or_else(|_| "2".to_string())
            .as_str()
        {
            "0" => SignatureType::Eoa,
            "1" => SignatureType::PolyProxy,
            "2" => SignatureType::PolyGnosisSafe,
            other => bail!("POLYMARKET_SIG_TYPE must be 0, 1, or 2 (got {other})"),
        };

        let default_size_usdc = std::env::var("DEFAULT_SIZE_USDC")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(5.0);

        // Per-side slippage; `MARKET_SLIPPAGE_BPS` sets both when the specific var is unset (legacy).
        let legacy_slippage_bps = std::env::var("MARKET_SLIPPAGE_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok());
        let market_buy_slippage_bps = std::env::var("MARKET_BUY_SLIPPAGE_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .or(legacy_slippage_bps)
            .unwrap_or(50);
        let market_sell_slippage_bps = std::env::var("MARKET_SELL_SLIPPAGE_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .or(legacy_slippage_bps)
            .unwrap_or(50);

        let market_buy_take_profit_bps = std::env::var("MARKET_BUY_TAKE_PROFIT_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        let market_buy_trail_bps = std::env::var("MARKET_BUY_TRAIL_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        let relayer_api_key = std::env::var("POLYMARKET_RELAYER_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let relayer_api_key_address = std::env::var("POLYMARKET_RELAYER_API_KEY_ADDRESS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| Address::from_str(&s))
            .transpose()
            .context("POLYMARKET_RELAYER_API_KEY_ADDRESS is not a valid address")?;

        let polygon_rpc_url = std::env::var("POLYGON_RPC_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://polygon-rpc.com".to_string());

        let signer: alloy_signer_local::PrivateKeySigner = private_key.parse()
            .context("Could not parse POLYMARKET_PK as a private key")?;
        let signer_address = signer.address();

        if signer_address != funder && sig_type == SignatureType::Eoa {
            bail!(
                "POLYMARKET_SIG_TYPE=0 (EOA) requires POLYMARKET_FUNDER to match the wallet derived from POLYMARKET_PK. \
                 Your funder is a different address (typically the Polymarket Gnosis Safe). Set POLYMARKET_SIG_TYPE=2."
            );
        }

        Ok(Self {
            private_key,
            funder,
            signer_address,
            sig_type,
            default_size_usdc,
            market_buy_slippage_bps,
            market_sell_slippage_bps,
            market_buy_take_profit_bps,
            market_buy_trail_bps,
            relayer_api_key,
            relayer_api_key_address,
            polygon_rpc_url,
        })
    }
}
