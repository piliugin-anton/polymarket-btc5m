//! Configuration: endpoints, env vars, chain constants.
//!
//! All trading-sensitive values come from environment variables. See `.env.example`.
//! Trailing width: prefer `BUY_TRAIL_BPS`; `MARKET_BUY_TRAIL_BPS` is a legacy alias when unset.
//! Take-profit / trail-activation bps: prefer `TAKE_PROFIT_BPS`; `MARKET_BUY_TAKE_PROFIT_BPS` is a legacy alias when unset.

use alloy_primitives::Address;
use anyhow::{bail, Context, Result};
use std::str::FromStr;

use crate::deposit_wallet::derive_deposit_wallet_address_polygon;

// ── Polymarket endpoints ────────────────────────────────────────────
pub const CLOB_HOST: &str = "https://clob.polymarket.com";
pub const GAMMA_HOST: &str = "https://gamma-api.polymarket.com";
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
pub const CTF_EXCHANGE_V1: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
pub const NEG_RISK_CTF_EXCHANGE_V1: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

// CTF Exchange — V2 EIP-712 (`domain.version` = "2") for when `GET /version` returns 2.
// https://docs.polymarket.com/v2-migration#for-api-users
pub const CTF_EXCHANGE_V2: &str = "0xE111180000d2663C0091e4f400237545B87B996B";
pub const NEG_RISK_CTF_EXCHANGE_V2: &str = "0xe2222d279d744050d28e00520010520000310F59";

/// Signature type (see CTF Exchange `OrderStructs.sol`).
///
/// * 0 = `EOA` — you hold a plain wallet and trade directly (rare on Polymarket).
/// * 1 = `POLY_PROXY` — legacy magic/email-based proxy wallet.
/// * 2 = `POLY_GNOSIS_SAFE` — the current default: your EOA owns a Gnosis Safe that
///   holds the USDC. `funder` = safe address, `signer` = EOA address.
/// * 3 = `POLY_1271` — deposit wallet ([Polymarket migration](https://docs.polymarket.com/trading/deposit-wallet-migration)):
///   `funder` = deterministic deposit wallet; orders use wrapped EIP-712 (`POLY_1271` signature type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    Eoa = 0,
    PolyProxy = 1,
    PolyGnosisSafe = 2,
    Poly1271 = 3,
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
    /// Default limit price for `[`/`]` quick-limit keys and the `l` modal pre-fill (`DEFAULT_PRICE`, 0.01–0.99).
    pub default_price: f64,
    /// Max fill slippage for market **buy** FAK orders (basis points; widens buy ceiling vs best ask).
    pub market_buy_slippage_bps: u32,
    /// Max fill slippage for market **sell** FAK orders (basis points; widens sell floor vs best bid).
    pub market_sell_slippage_bps: u32,
    /// Bps margin: (1) with trailing **off** — GTD SELL after a **Buy** (FAK market or GTD limit with
    /// fill) at this edge ([`crate::fees::take_profit_limit_price_crypto_after_fees`]). (2) with
    /// trailing **on** — trail **arms** when `best_bid >= entry * (1 + bps/10_000)` with `entry` from
    /// the live position (`avg_entry`) or the fill estimate until the position is applied (`0` =
    /// arm as soon as bid reaches entry).
    pub take_profit_bps: u32,
    /// If positive after a **Buy** (FAK market or GTD limit when the POST response includes a fill),
    /// run a trailing stop on CLOB best bid, then FAK SELL; trail width in bps from peak. Resting
    /// limit buys arm the same trail when the fill arrives on the user channel as a **maker** leg
    /// (`BUY_TRAIL_BPS` at startup, copied onto each [`crate::app::AppEvent::MarketRoll`]).
    pub buy_trail_bps: u32,
    /// Trailing FAK **sell** floor (`best_bid × (1 − sell slippage bps)`) must be at least this many
    /// **gross** basis points above cost basis (`avg_entry` or trail install entry) before `place_order`.
    /// `0` = no gate (legacy). If unset entry cannot be resolved, the sell is not blocked.
    pub trailing_exit_min_profit_bps: u32,
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
            "3" => SignatureType::Poly1271,
            other => bail!("POLYMARKET_SIG_TYPE must be 0, 1, 2, or 3 (got {other})"),
        };

        let default_size_usdc = std::env::var("DEFAULT_SIZE_USDC")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(5.0);

        let default_price = std::env::var("DEFAULT_PRICE")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|p| (0.01..=0.99).contains(p))
            .unwrap_or(0.50);

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

        let take_profit_bps = std::env::var("TAKE_PROFIT_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .or_else(|| {
                std::env::var("MARKET_BUY_TAKE_PROFIT_BPS")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
            })
            .unwrap_or(0);

        let buy_trail_bps = std::env::var("BUY_TRAIL_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .or_else(|| {
                std::env::var("MARKET_BUY_TRAIL_BPS")
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
            })
            .unwrap_or(0);

        let trailing_exit_min_profit_bps = std::env::var("TRAILING_EXIT_MIN_PROFIT_BPS")
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

        let signer: alloy_signer_local::PrivateKeySigner = private_key
            .parse()
            .context("Could not parse POLYMARKET_PK as a private key")?;
        let signer_address = signer.address();

        if sig_type == SignatureType::Poly1271 {
            let expected = derive_deposit_wallet_address_polygon(signer_address);
            if funder != expected {
                bail!(
                    "POLYMARKET_SIG_TYPE=3 (deposit wallet / POLY_1271) requires POLYMARKET_FUNDER ({:#x}) \
                     to equal the deterministic deposit wallet for this EOA ({:#x}).",
                    funder,
                    expected
                );
            }
        }

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
            default_price,
            market_buy_slippage_bps,
            market_sell_slippage_bps,
            take_profit_bps,
            buy_trail_bps,
            trailing_exit_min_profit_bps,
            relayer_api_key,
            relayer_api_key_address,
            polygon_rpc_url,
        })
    }
}

#[cfg(test)]
fn parse_env_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_env_bool;

    #[test]
    fn parse_env_bool_accepts_common_values() {
        assert_eq!(parse_env_bool("true"), Some(true));
        assert_eq!(parse_env_bool("1"), Some(true));
        assert_eq!(parse_env_bool("off"), Some(false));
        assert_eq!(parse_env_bool("FALSE"), Some(false));
        assert_eq!(parse_env_bool("maybe"), None);
    }
}
