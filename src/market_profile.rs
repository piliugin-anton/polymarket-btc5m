//! User-selected crypto asset + time horizon for rolling Chainlink up/down markets.
//!
//! **Rolling** (5m / 15m): slugs are `{asset}-updown-{5m|15m}-{window_start_unix}`.
//! **Daily** (1D): slugs are date-based, e.g. `bitcoin-up-or-down-on-april-22-2026` in America/New_York.

use chrono::Datelike;

/// Known spot assets matching Polymarket recurring crypto up/down + RTDS + crypto-price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoAsset {
    pub label:                    &'static str,
    /// First segment in rolling slugs: `btc` → `btc-updown-5m-…`.
    pub rolling_slug_prefix:     &'static str,
    /// Prefix for daily calendar events: `bitcoin-up-or-down-on` → `bitcoin-up-or-down-on-april-22-2026`.
    pub daily_event_prefix:      &'static str,
    /// Polymarket RTDS `crypto_prices_chainlink` filter, e.g. `btc/usd`.
    pub rtds_symbol:             &'static str,
    /// Chainlink Data Streams feed ID (mainnet Premium Global crypto / USD). Used when
    /// `CHAINLINK_API_KEY` and `CHAINLINK_USER_SECRET` are set. IDs match
    /// [data.chain.link/streams](https://data.chain.link/streams).
    pub data_streams_feed_id_hex: &'static str,
    /// `symbol` for `/api/crypto/crypto-price` (uppercase), e.g. `BTC`.
    pub crypto_price_symbol:     &'static str,
    /// `GET /series?slug=…&exclude_events=true` for wizard list (5m title / volume).
    pub series_slug_5m:          &'static str,
}

/// Lookup Chainlink Data Streams feed id for a Polymarket RTDS symbol (e.g. `eth/usd`).
pub fn data_streams_feed_id_for_rtds_symbol(rtds_symbol: &str) -> Option<&'static str> {
    CRYPTO_ASSETS
        .iter()
        .find(|a| a.rtds_symbol.eq_ignore_ascii_case(rtds_symbol))
        .map(|a| a.data_streams_feed_id_hex)
}

pub const CRYPTO_ASSETS: &[CryptoAsset] = &[
    CryptoAsset {
        label:                "BTC",
        rolling_slug_prefix:  "btc",
        daily_event_prefix:   "bitcoin-up-or-down-on",
        rtds_symbol:          "btc/usd",
        data_streams_feed_id_hex: "0x00039d9e45394f473ab1f050a1b963e6b05351e52d71e507509ada0c95ed75b8",
        crypto_price_symbol:  "BTC",
        series_slug_5m:       "btc-up-or-down-5m",
    },
    CryptoAsset {
        label:                "ETH",
        rolling_slug_prefix:  "eth",
        daily_event_prefix:   "ethereum-up-or-down-on",
        rtds_symbol:          "eth/usd",
        data_streams_feed_id_hex: "0x000362205e10b3a147d02792eccee483dca6c7b44ecce7012cb8c6e0b68b3ae9",
        crypto_price_symbol:  "ETH",
        series_slug_5m:       "eth-up-or-down-5m",
    },
    CryptoAsset {
        label:                "SOL",
        rolling_slug_prefix:  "sol",
        daily_event_prefix:   "solana-up-or-down-on",
        rtds_symbol:          "sol/usd",
        data_streams_feed_id_hex: "0x0003b778d3f6b2ac4991302b89cb313f99a42467d6c9c5f96f57c29c0d2bc24f",
        crypto_price_symbol:  "SOL",
        series_slug_5m:       "sol-up-or-down-5m",
    },
    CryptoAsset {
        label:                "XRP",
        rolling_slug_prefix:  "xrp",
        daily_event_prefix:   "xrp-up-or-down-on",
        rtds_symbol:          "xrp/usd",
        data_streams_feed_id_hex: "0x0003c16c6aed42294f5cb4741f6e59ba2d728f0eae2eb9e6d3f555808c59fc45",
        crypto_price_symbol:  "XRP",
        series_slug_5m:       "xrp-up-or-down-5m",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timeframe {
    /// 5-minute rolling grid (300s).
    M5,
    /// 15-minute rolling grid (900s).
    M15,
    /// One market per **calendar day** in `America/New_York` (slug pattern `…-on-april-22-2026`).
    D1,
}

impl Timeframe {
    pub fn label(self) -> &'static str {
        match self {
            Timeframe::M5  => "5m",
            Timeframe::M15 => "15m",
            Timeframe::D1  => "1 day",
        }
    }

    /// Wording for TUI titles (e.g. header block: "BTC 5 minutes").
    pub fn tui_phrase(self) -> &'static str {
        match self {
            Timeframe::M5  => "5 minutes",
            Timeframe::M15 => "15 minutes",
            Timeframe::D1  => "1 day",
        }
    }

    /// Rolling window length in seconds (only for [`Timeframe::M5`] / [`Timeframe::M15`]).
    pub fn window_sec_rolling(self) -> Option<i64> {
        match self {
            Timeframe::M5  => Some(300),
            Timeframe::M15 => Some(900),
            Timeframe::D1  => None,
        }
    }

    /// Middle segment in rolling slugs: `5m` or `15m`.
    pub fn rolling_slug_token(self) -> Option<&'static str> {
        match self {
            Timeframe::M5  => Some("5m"),
            Timeframe::M15 => Some("15m"),
            Timeframe::D1  => None,
        }
    }

    /// `variant` query for Polymarket `/api/crypto/crypto-price`.
    ///
    /// Always `fiveminute`: Polymarket’s `fifteenminute` / `daily` responses are often
    /// `incomplete: true` with a stale or wrong `openPrice`, while `fiveminute` returns
    /// `completed: true` for the same Gamma window (15m and noon-to-noon daily are 5m-aligned).
    pub fn crypto_price_variant(self) -> &'static str {
        "fiveminute"
    }
}

/// What the app trades after the wizard: one asset and one time horizon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketProfile {
    pub asset:     CryptoAsset,
    pub timeframe: Timeframe,
}

impl MarketProfile {
    pub fn rolling_slug_for_window_start(self: &Self, window_start_ts: i64) -> Option<String> {
        let _ = self.timeframe.window_sec_rolling()?;
        let token = self.timeframe.rolling_slug_token()?;
        Some(format!(
            "{}-updown-{}-{}",
            self.asset.rolling_slug_prefix, token, window_start_ts
        ))
    }

    /// Whether discovery uses the rolling 5m/15m grid (vs calendar daily slug).
    pub fn is_rolling(self: &Self) -> bool {
        self.timeframe.window_sec_rolling().is_some()
    }
}

/// Build `bitcoin-up-or-down-on-april-22-2026` (Polymarket calendar-day URL slug, English month, ET calendar).
pub fn build_daily_event_slug(daily_event_prefix: &str, ny_date: chrono::NaiveDate) -> String {
    let m = month_name_en_lower(ny_date.month());
    format!("{}-{}-{}-{}", daily_event_prefix, m, ny_date.day(), ny_date.year())
}

fn month_name_en_lower(m: u32) -> &'static str {
    match m {
        1  => "january",
        2  => "february",
        3  => "march",
        4  => "april",
        5  => "may",
        6  => "june",
        7  => "july",
        8  => "august",
        9  => "september",
        10 => "october",
        11 => "november",
        12 => "december",
        _  => "january",
    }
}
