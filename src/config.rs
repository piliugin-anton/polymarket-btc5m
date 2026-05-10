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
    /// When true, STRONG strategy signals can submit automatic **GTD limit BUY** orders at the
    /// snapshot best ask (same sizing as manual limit buys).
    pub autotrading: bool,
    /// Maximum currently open auto-trading positions. Closed auto inventory frees capacity.
    pub autotrading_max_positions: usize,
    /// When set, autotrading GTD BUY `expiration` is `now + this many seconds` (plus Polymarket’s
    /// +60s signing offset — see [`crate::gamma::clob_gtd_expiration_secs_after_duration_from_now`]).
    /// When unset, expiration matches manual limits: end of the current market window.
    pub autotrading_order_expires_after_secs: Option<u64>,
    /// Optional safety cap for automatic GTD limit BUY entries. When set, autotrading skips a
    /// STRONG signal if the snapshot best ask is above this price.
    pub autotrading_max_entry_price: Option<f64>,
    /// Multiplier on the required spot-vs-Price-to-Beat gap for a `STRONG` signal (default `1.0`).
    /// Clamped to roughly `[0.55, 1.15]` — values below `1.0` fire more often but are less selective.
    pub strategy_strong_gap_mult: f64,
    /// Multiplier on the max bid–ask spread allowed for strategy book checks (default `1.0`, max ~`1.35`).
    /// Above `1.0` admits wider books → more signals, worse execution risk.
    pub strategy_max_spread_mult: f64,
    /// Minimum best-ask size (shares) for the strategy to treat a book as usable (default `5.0`).
    /// Lower values (e.g. `3.0`) admit thinner liquidity.
    pub strategy_min_top_ask_shares: f64,
    /// `WATCH` when `gap >= strong_gap * ratio` (default `0.60`). Lower → more `WATCH` labels.
    pub strategy_watch_ratio: f64,
    /// Polymarket Relayer API key (Settings → API) — required for gasless Safe `execTransaction` (CTF redeem).
    pub relayer_api_key: Option<String>,
    /// Address paired with the relayer API key (same screen in Polymarket settings).
    pub relayer_api_key_address: Option<Address>,
    /// Polygon JSON-RPC URL for `eth_call` (neg-risk balances). Public default if unset.
    pub polygon_rpc_url: String,

    /// When true, append JSONL round logs under [`Self::round_log_dir`].
    pub round_log_enabled: bool,
    /// Directory for daily `YYYY-MM-DD.jsonl` session files.
    pub round_log_dir: std::path::PathBuf,
    /// Minimum seconds between `snap` lines while a round is active (clamped 2–120).
    pub round_log_snap_interval_secs: u64,
    /// When true and round logging is enabled, also append `fill` lines (diagnostics).
    pub round_log_fills: bool,
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

        let autotrading = std::env::var("AUTOTRADING")
            .ok()
            .as_deref()
            .and_then(parse_env_bool)
            .unwrap_or(false);
        let autotrading_max_positions = parse_autotrading_max_positions(
            std::env::var("AUTOTRADING_MAX_POSITIONS").ok().as_deref(),
        );
        let autotrading_order_expires_after_secs = parse_autotrading_order_expires_after_secs(
            std::env::var("AUTOTRADING_ORDER_EXPIRES_AFTER")
                .ok()
                .as_deref(),
        );
        let autotrading_max_entry_price = parse_autotrading_max_entry_price(
            std::env::var("AUTOTRADING_MAX_ENTRY_PRICE").ok().as_deref(),
        );

        let strategy_strong_gap_mult = parse_strategy_strong_gap_mult(
            std::env::var("STRATEGY_STRONG_GAP_MULT").ok().as_deref(),
        );
        let strategy_max_spread_mult = parse_strategy_max_spread_mult(
            std::env::var("STRATEGY_MAX_SPREAD_MULT").ok().as_deref(),
        );
        let strategy_min_top_ask_shares = parse_strategy_min_top_ask_shares(
            std::env::var("STRATEGY_MIN_TOP_ASK_SHARES").ok().as_deref(),
        );
        let strategy_watch_ratio =
            parse_strategy_watch_ratio(std::env::var("STRATEGY_WATCH_RATIO").ok().as_deref());

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

        let round_log_enabled = std::env::var("ROUND_LOG_ENABLED")
            .ok()
            .as_deref()
            .and_then(parse_env_bool)
            .unwrap_or(false);
        let round_log_dir = std::env::var("ROUND_LOG_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("./data/rounds"));
        let round_log_snap_interval_secs = parse_round_log_snap_interval_secs(
            std::env::var("ROUND_LOG_SNAP_INTERVAL_SECS")
                .ok()
                .as_deref(),
        );
        let round_log_fills = std::env::var("ROUND_LOG_FILLS")
            .ok()
            .as_deref()
            .and_then(parse_env_bool)
            .unwrap_or(false);

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
            autotrading,
            autotrading_max_positions,
            autotrading_order_expires_after_secs,
            autotrading_max_entry_price,
            strategy_strong_gap_mult,
            strategy_max_spread_mult,
            strategy_min_top_ask_shares,
            strategy_watch_ratio,
            relayer_api_key,
            relayer_api_key_address,
            polygon_rpc_url,
            round_log_enabled,
            round_log_dir,
            round_log_snap_interval_secs,
            round_log_fills,
        })
    }
}

fn parse_round_log_snap_interval_secs(value: Option<&str>) -> u64 {
    const DEFAULT: u64 = 10;
    let Some(raw) = value.and_then(|s| s.trim().parse::<u64>().ok()) else {
        return DEFAULT;
    };
    raw.clamp(2, 120)
}

fn parse_env_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_autotrading_max_positions(value: Option<&str>) -> usize {
    value
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1)
}

fn parse_autotrading_order_expires_after_secs(value: Option<&str>) -> Option<u64> {
    value
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
}

fn parse_autotrading_max_entry_price(value: Option<&str>) -> Option<f64> {
    value
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|p| p.is_finite() && (0.01..=0.99).contains(p))
}

fn parse_strategy_strong_gap_mult(value: Option<&str>) -> f64 {
    const DEFAULT: f64 = 1.0;
    let Some(raw) = value.and_then(|s| s.trim().parse::<f64>().ok()) else {
        return DEFAULT;
    };
    if !raw.is_finite() {
        return DEFAULT;
    }
    raw.clamp(0.55, 1.15)
}

fn parse_strategy_max_spread_mult(value: Option<&str>) -> f64 {
    const DEFAULT: f64 = 1.0;
    let Some(raw) = value.and_then(|s| s.trim().parse::<f64>().ok()) else {
        return DEFAULT;
    };
    if !raw.is_finite() {
        return DEFAULT;
    }
    raw.clamp(1.0, 1.35)
}

fn parse_strategy_min_top_ask_shares(value: Option<&str>) -> f64 {
    const DEFAULT: f64 = 5.0;
    let Some(raw) = value.and_then(|s| s.trim().parse::<f64>().ok()) else {
        return DEFAULT;
    };
    if !raw.is_finite() {
        return DEFAULT;
    }
    raw.clamp(2.0, 50.0)
}

fn parse_strategy_watch_ratio(value: Option<&str>) -> f64 {
    const DEFAULT: f64 = 0.60;
    let Some(raw) = value.and_then(|s| s.trim().parse::<f64>().ok()) else {
        return DEFAULT;
    };
    if !raw.is_finite() {
        return DEFAULT;
    }
    raw.clamp(0.40, 0.85)
}

#[cfg(test)]
mod tests {
    use super::{
        parse_autotrading_max_entry_price, parse_autotrading_max_positions,
        parse_autotrading_order_expires_after_secs, parse_env_bool,
        parse_round_log_snap_interval_secs, parse_strategy_max_spread_mult,
        parse_strategy_min_top_ask_shares, parse_strategy_strong_gap_mult,
        parse_strategy_watch_ratio,
    };

    #[test]
    fn parse_env_bool_accepts_common_values() {
        assert_eq!(parse_env_bool("true"), Some(true));
        assert_eq!(parse_env_bool("1"), Some(true));
        assert_eq!(parse_env_bool("off"), Some(false));
        assert_eq!(parse_env_bool("FALSE"), Some(false));
        assert_eq!(parse_env_bool("maybe"), None);
    }

    #[test]
    fn parse_autotrading_max_positions_defaults_invalid_values_to_one() {
        assert_eq!(parse_autotrading_max_positions(None), 1);
        assert_eq!(parse_autotrading_max_positions(Some("")), 1);
        assert_eq!(parse_autotrading_max_positions(Some("0")), 1);
        assert_eq!(parse_autotrading_max_positions(Some("not-a-number")), 1);
        assert_eq!(parse_autotrading_max_positions(Some("3")), 3);
    }

    #[test]
    fn parse_autotrading_order_expires_after_secs_accepts_positive_only() {
        assert_eq!(parse_autotrading_order_expires_after_secs(None), None);
        assert_eq!(parse_autotrading_order_expires_after_secs(Some("")), None);
        assert_eq!(parse_autotrading_order_expires_after_secs(Some("0")), None);
        assert_eq!(
            parse_autotrading_order_expires_after_secs(Some("not-a-number")),
            None
        );
        assert_eq!(
            parse_autotrading_order_expires_after_secs(Some("120")),
            Some(120)
        );
    }

    #[test]
    fn parse_autotrading_max_entry_price_accepts_valid_probability_prices() {
        assert_eq!(parse_autotrading_max_entry_price(None), None);
        assert_eq!(parse_autotrading_max_entry_price(Some("")), None);
        assert_eq!(
            parse_autotrading_max_entry_price(Some("not-a-number")),
            None
        );
        assert_eq!(parse_autotrading_max_entry_price(Some("0")), None);
        assert_eq!(parse_autotrading_max_entry_price(Some("1.0")), None);
        assert_eq!(parse_autotrading_max_entry_price(Some("0.95")), Some(0.95));
        assert_eq!(parse_autotrading_max_entry_price(Some("0.99")), Some(0.99));
    }

    #[test]
    fn parse_strategy_tuners_clamp_and_default() {
        assert_eq!(parse_strategy_strong_gap_mult(None), 1.0);
        assert_eq!(parse_strategy_strong_gap_mult(Some("0.8")), 0.8);
        assert_eq!(parse_strategy_strong_gap_mult(Some("0.2")), 0.55);
        assert_eq!(parse_strategy_strong_gap_mult(Some("2")), 1.15);

        assert_eq!(parse_strategy_max_spread_mult(None), 1.0);
        assert_eq!(parse_strategy_max_spread_mult(Some("1.2")), 1.2);
        assert_eq!(parse_strategy_max_spread_mult(Some("0.5")), 1.0);

        assert_eq!(parse_strategy_min_top_ask_shares(None), 5.0);
        assert_eq!(parse_strategy_min_top_ask_shares(Some("3")), 3.0);
        assert_eq!(parse_strategy_min_top_ask_shares(Some("1")), 2.0);

        assert!((parse_strategy_watch_ratio(Some("0.5")) - 0.5).abs() < 1e-9);
        assert!((parse_strategy_watch_ratio(Some("0.2")) - 0.40).abs() < 1e-9);
    }

    #[test]
    fn parse_round_log_snap_interval_secs_clamps() {
        assert_eq!(parse_round_log_snap_interval_secs(None), 10);
        assert_eq!(parse_round_log_snap_interval_secs(Some("10")), 10);
        assert_eq!(parse_round_log_snap_interval_secs(Some("1")), 2);
        assert_eq!(parse_round_log_snap_interval_secs(Some("500")), 120);
    }
}
