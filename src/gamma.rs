//! Gamma API client.
//!
//! Used exclusively for market *discovery*: the app’s Gamma poll task calls into
//! this no-auth REST API on a **fixed in-window interval** ([`crate::feeds::market_discovery_gamma::GAMMA_POLL_IN_WINDOW_SECS`]); **after** `closes_at` the
//! discovery task polls only **`/markets/slug/btc-updown-5m-{start+300}`** (next window) once per
//! second until the slug resolves (see [`crate::feeds::market_discovery_gamma`]) — no broad search.
//! Requests are mutex-serialized. The initial lookup still uses a multi-slug search around
//! “now” via [`GammaClient::find_current_btc_5m`]. Used to find the
//! currently-active "Bitcoin Up or Down - 5m" market and extract
//! the UP / DOWN token IDs, the tick size, neg_risk flag, and the "Price to Beat".
//!
//! Market slug pattern:
//!   btc-updown-5m-<unix-timestamp-of-window-start>
//!
//! Each market has a single event with two outcome tokens (Up / Down).
//!
//! Discovery uses **`GET /markets/slug/btc-updown-5m-{ts}`** (single market object).
//! Polymarket documents slug lookup as path-based `/markets/slug/` and `/events/slug/`
//! rather than `?slug=` filters; the latter can mis-match recurring 5m markets.
//! See [Fetch markets guide](https://docs.polymarket.com/developers/gamma-markets-api/fetch-markets-guide).
//!
//! **Price to beat (opening USD)** — prefer Polymarket’s
//! [`crate::config::POLYMARKET_CRYPTO_PRICE_URL`] (`openPrice`), then description
//! parsing, then the app’s RTDS latch.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::StatusCode;
use serde::Deserialize;
use tracing::debug;

use crate::trading::canonical_clob_token_id;
use url::Url;

use chrono_tz::America::New_York;

use crate::config::{GAMMA_HOST, POLYMARKET_CRYPTO_PRICE_URL};
use crate::market_profile::{
    build_daily_event_slug, MarketProfile, Timeframe, CRYPTO_ASSETS,
};

/// Five-minute window length in seconds (matches Polymarket BTC 5m slugs).
pub const BTC_5M_WINDOW_SEC: i64 = 300;

/// Gamma returns `orderPriceMinTickSize` as JSON number; `format!("{v}")` on `f64` can emit IEEE
/// tails (e.g. `0.010000000000000009`). Trim to a short decimal so CLOB rounding config matches
/// Polymarket `ROUNDING_CONFIG` keys (`"0.01"`, `"0.001"`, …).
fn gamma_tick_f64_to_string(v: f64) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "0.01".into();
    }
    let s = format!("{:.12}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0.01".into()
    } else {
        s.to_string()
    }
}

/// What the rest of the app needs to know about a market.
#[derive(Debug, Clone)]
pub struct ActiveMarket {
    #[allow(dead_code)]
    pub condition_id:   String,
    pub question:       String,
    pub slug:           String,
    pub up_token_id:    String,
    pub down_token_id:  String,
    pub tick_size:      String,   // e.g. "0.01" — CLOB rounding (`ROUNDING_CONFIG`)
    pub neg_risk:       bool,
    pub price_to_beat:  Option<f64>, // Polymarket crypto-price API, else description, else RTDS latch in app
    pub opens_at:       DateTime<Utc>,
    pub closes_at:      DateTime<Utc>,
    /// UTC `Z` strings passed as `eventStartTime` / `endDate` to Polymarket crypto-price (debug / TUI).
    pub crypto_price_query_start_utc: String,
    pub crypto_price_query_end_utc:   String,
}

/// Signed CLOB `expiration` (UTC unix seconds) for a **GTD** limit that should stop resting at
/// the 5m window end (`window_end`).
///
/// Polymarket applies a **+60s security offset** on GTD: resting ends around `expiration - 60`.
/// So we use `window_end + 60` as the signed field. See Polymarket “Create Order” → GTD.
pub fn clob_gtd_expiration_secs_at_window_end(window_end: DateTime<Utc>) -> Result<u64> {
    let target = window_end;
    let now = Utc::now();
    if target <= now {
        bail!(“too close to window end for a GTD limit (window must end in the future)”);
    }
    let signed = target
        .timestamp()
        .checked_add(60)
        .filter(|&s| s > now.timestamp())
        .ok_or_else(|| anyhow!("could not compute Polymarket GTD expiration (time overflow or past)"))?;
    Ok(signed as u64)
}

// ── Raw Gamma JSON shapes ───────────────────────────────────────────
// The Gamma API returns fields in mixed camelCase / snake_case; we
// deserialise with serde(rename_all) and keep only what we actually use.

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(default)]
    #[allow(dead_code)]
    title:     String,
    #[serde(default)]
    #[allow(dead_code)]
    slug:      String,
    #[serde(default, rename = "startDate")] start_date: Option<String>,
    #[serde(default, rename = "endDate")]   end_date:   Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    markets: Vec<RawMarket>,
}

#[derive(Debug, Deserialize)]
struct CryptoPriceResponse {
    #[serde(default, rename = "openPrice")]
    open_price: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawMarket {
    #[serde(rename = "conditionId")] condition_id: String,
    #[serde(default)] question: String,
    #[serde(default)] slug:     String,
    /// Newline-separated JSON array `["tokenIdUp","tokenIdDown"]`.
    #[serde(default, rename = "clobTokenIds")] clob_token_ids: String,
    #[serde(default)] outcomes:       String, // e.g. `["Up","Down"]`
    #[serde(default, rename = "orderPriceMinTickSize")] tick_size_f: Option<f64>,
    #[serde(default, rename = "negRisk")] neg_risk: bool,
    #[serde(default)] description:    String,
    #[serde(default, rename = "startDate")] start_date: Option<String>,
    #[serde(default, rename = "endDate")]   end_date:   Option<String>,
    /// Trading-window start on slug responses (preferred over `startDate` when present).
    #[serde(default, rename = "eventStartTime")] event_start_time: Option<String>,
    #[serde(default)] closed: bool,
}

pub struct GammaClient {
    http: reqwest::Client,
}

impl GammaClient {
    pub fn new() -> Self {
        Self {
            http: crate::net::reqwest_client().expect("reqwest client"),
        }
    }

    /// Default BTC 5m discovery (kept for external scripts / back-compat).
    #[allow(dead_code)]
    pub async fn find_current_btc_5m(&self) -> Result<ActiveMarket> {
        let p = MarketProfile {
            asset:     CRYPTO_ASSETS[0].clone(),
            timeframe: Timeframe::M5,
        };
        self.find_current_updown(&p).await
    }

    /// Find the up/down market for `profile` (rolling 5m/15m or daily calendar in ET).
    pub async fn find_current_updown(&self, profile: &MarketProfile) -> Result<ActiveMarket> {
        if profile.is_rolling() {
            self.find_current_rolling_updown(profile).await
        } else {
            self.find_current_daily_updown(profile).await
        }
    }

    /// Align `now` to the step grid, then request `/markets/slug/{…-updown-5m-…}` for that window
    /// and neighbors. Pick the market where `start <= now < end`, else the next upcoming window.
    async fn find_current_rolling_updown(&self, profile: &MarketProfile) -> Result<ActiveMarket> {
        let step = profile
            .timeframe
            .window_sec_rolling()
            .ok_or_else(|| anyhow!("not a rolling profile"))?;
        let now = Utc::now();
        let now_ts = now.timestamp();
        let base = (now_ts / step) * step;

        let mut candidates: Vec<RawMarket> = Vec::new();
        for off in [0, step, -step, 2 * step, -2 * step] {
            let ts = base + off;
            let Some(slug) = profile.rolling_slug_for_window_start(ts) else { continue; };
            let url = format!("{GAMMA_HOST}/markets/slug/{slug}");
            debug!(%url, "gamma: fetch market by slug");

            let resp = self
                .http
                .get(&url)
                .send()
                .await
                .with_context(|| format!("gamma GET {url} failed"))?;

            if resp.status() == StatusCode::NOT_FOUND {
                continue;
            }

            let m: RawMarket = resp
                .error_for_status()
                .with_context(|| format!("gamma GET {url} bad status"))?
                .json()
                .await
                .with_context(|| format!("gamma GET {url} decode failed"))?;

            if m.closed {
                continue;
            }
            candidates.push(m);
        }

        let pfx = format!(
            "{}-updown-{}-",
            profile.asset.rolling_slug_prefix,
            profile.timeframe.rolling_slug_token().unwrap_or("5m")
        );
        candidates.sort_by_key(|m| {
            m.slug
                .strip_prefix(&pfx)
                .and_then(|t| t.parse::<i64>().ok())
                .unwrap_or(i64::MAX)
        });

        let market = candidates
            .iter()
            .find(|m| {
                match (gamma_market_window_start(m), parse_ts(m.end_date.as_deref())) {
                    (Some(s), Some(c)) => s <= now && now < c,
                    _ => false,
                }
            })
            .or_else(|| {
                candidates.iter().find(|m| {
                    gamma_market_window_start(m).is_some_and(|s| s >= now)
                })
            })
            .ok_or_else(|| {
                anyhow!(
                    "no active or upcoming {pfx}* market (tried windows around unix {base})",
                    pfx = pfx,
                    base = base
                )
            })?;

        self.active_market_from_raw_with_crypto(market, profile).await
    }

    /// `GET /markets/slug/…-updown-…- {window_start_ts}` — for fast roll polling.
    /// Returns `Ok(None)` on 404 or if Gamma marks the market closed (not ready yet).
    pub async fn try_fetch_rolling_by_window_start_ts(
        &self,
        window_start_ts: i64,
        profile:         &MarketProfile,
    ) -> Result<Option<ActiveMarket>> {
        if !profile.is_rolling() {
            bail!("not a rolling profile");
        }
        let Some(slug) = profile.rolling_slug_for_window_start(window_start_ts) else {
            bail!("invalid rolling profile");
        };
        let url = format!("{GAMMA_HOST}/markets/slug/{slug}");
        debug!(%url, "gamma: fetch next window by slug");

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("gamma GET {url} failed"))?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let m: RawMarket = resp
            .error_for_status()
            .with_context(|| format!("gamma GET {url} bad status"))?
            .json()
            .await
            .with_context(|| format!("gamma GET {url} decode failed"))?;

        if m.closed {
            return Ok(None);
        }

        self.active_market_from_raw_with_crypto(&m, profile).await.map(Some)
    }

    /// Calendar-day markets in ET: `bitcoin-up-or-down-on-april-22-2026` etc.
    async fn find_current_daily_updown(&self, profile: &MarketProfile) -> Result<ActiveMarket> {
        if profile.timeframe != Timeframe::D1 {
            bail!("not daily timeframe");
        }
        let now_ny = Utc::now().with_timezone(&New_York);
        let today = now_ny.date_naive();
        let mut d = today;
        for _ in 0..5 {
            let slug = build_daily_event_slug(profile.asset.daily_event_prefix, d);
            if let Some(m) = self.fetch_open_raw_market_by_slug(&slug).await? {
                return self.active_market_from_raw_with_crypto(&m, profile).await;
            }
            d += chrono::Duration::days(1);
        }
        anyhow::bail!("no open daily up/down market (tried a few days from {today} for this asset)");
    }

    /// Next calendar day in ET after the previous market's end — poll for the next `…-on-mmm-d-yyyy` slug.
    pub async fn try_fetch_next_daily_by_previous_close(
        &self,
        profile:             &MarketProfile,
        last_window_end_utc: DateTime<Utc>,
    ) -> Result<Option<ActiveMarket>> {
        if profile.timeframe != Timeframe::D1 {
            bail!("not daily timeframe");
        }
        let d0 = last_window_end_utc.with_timezone(&New_York).date_naive();
        let start_date = d0 + chrono::Duration::days(1);
        for add in 0i64..10i64 {
            let d = start_date + chrono::Duration::days(add);
            let slug = build_daily_event_slug(profile.asset.daily_event_prefix, d);
            if let Some(m) = self.fetch_open_raw_market_by_slug(&slug).await? {
                return self.active_market_from_raw_with_crypto(&m, profile).await.map(Some);
            }
        }
        Ok(None)
    }

    async fn fetch_open_raw_market_by_slug(&self, slug: &str) -> Result<Option<RawMarket>> {
        let url = format!("{GAMMA_HOST}/markets/slug/{slug}");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("gamma GET {url} failed"))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let m: RawMarket = resp
            .error_for_status()
            .with_context(|| format!("gamma GET {url} bad status"))?
            .json()
            .await
            .with_context(|| format!("gamma GET {url} decode failed"))?;
        if m.closed {
            return Ok(None);
        }
        Ok(Some(m))
    }

    async fn active_market_from_raw_with_crypto(
        &self,
        market:  &RawMarket,
        profile: &MarketProfile,
    ) -> Result<ActiveMarket> {
        let mut m = active_market_from_raw_market(market, None)?;
        if profile.is_rolling() {
            let (win_start, win_end) = utc_rolling_window_bounds(&m.slug, m.opens_at, profile);
            m.opens_at = win_start;
            m.closes_at = win_end;
        } else {
            // Daily: trust Gamma `eventStartTime` / `endDate` (already in `m`).
        }
        let event_start = format_crypto_price_query_time(m.opens_at);
        let end_date = format_crypto_price_query_time(m.closes_at);
        m.crypto_price_query_start_utc = event_start.clone();
        m.crypto_price_query_end_utc = end_date.clone();
        if let Some(open) = self
            .fetch_polymarket_open_price(
                profile.asset.crypto_price_symbol,
                &event_start,
                &end_date,
                profile.timeframe.crypto_price_variant(),
            )
            .await
        {
            m.price_to_beat = Some(open);
        }
        Ok(m)
    }

    /// `GET /api/crypto/crypto-price` — `openPrice` for the window.
    async fn fetch_polymarket_open_price(
        &self,
        symbol:        &str,
        event_start:   &str,
        end_date:      &str,
        variant:       &str,
    ) -> Option<f64> {
        let mut u = Url::parse(POLYMARKET_CRYPTO_PRICE_URL).expect("POLYMARKET_CRYPTO_PRICE_URL");
        u.query_pairs_mut()
            .append_pair("symbol", symbol)
            .append_pair("eventStartTime", event_start)
            .append_pair("variant", variant)
            .append_pair("endDate", end_date);
        let res = self.http.get(u.as_str()).send()
            .await
            .map_err(|e| {
                debug!(error = %e, url = %u, "polymarket crypto-price request failed");
                e
            })
            .ok()?;

        if !res.status().is_success() {
            debug!(
                status = %res.status(),
                url = %u,
                "polymarket crypto-price bad status",
            );
            return None;
        }

        let body: CryptoPriceResponse = res.json().await.map_err(|e| {
            debug!(error = %e, "polymarket crypto-price decode failed");
            e
        }).ok()?;

        if let Some(p) = body.open_price {
            debug!(
                open_price = p,
                start = %event_start,
                end = %end_date,
                "polymarket crypto-price openPrice",
            );
        }
        body.open_price
    }
}

/// Polymarket `/api/crypto/crypto-price` query times use **UTC with `Z`** suffix
/// (e.g. `2026-04-18T18:50:00Z`), matching the site’s API URLs.
fn format_crypto_price_query_time(utc: DateTime<Utc>) -> String {
    utc.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Rolling windows: prefer slug tail `{prefix}-updown-{5m|15m}-{unix}`; else floor `opens_at` to the step grid.
fn utc_rolling_window_bounds(
    slug:     &str,
    opens_at: DateTime<Utc>,
    profile:  &MarketProfile,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let step = profile
        .timeframe
        .window_sec_rolling()
        .unwrap_or(BTC_5M_WINDOW_SEC);
    let pfx = format!(
        "{}-updown-{}-",
        profile.asset.rolling_slug_prefix,
        profile.timeframe.rolling_slug_token().unwrap_or("5m")
    );
    let start = if let Some(tail) = slug.strip_prefix(&pfx) {
        if let Ok(ts) = tail.parse::<i64>() {
            DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or(opens_at)
        } else {
            floor_utc_to_step(opens_at, step)
        }
    } else {
        floor_utc_to_step(opens_at, step)
    };
    let end = start + Duration::seconds(step);
    (start, end)
}

fn floor_utc_to_step(t: DateTime<Utc>, step: i64) -> DateTime<Utc> {
    let floored = (t.timestamp() / step) * step;
    DateTime::<Utc>::from_timestamp(floored, 0).unwrap_or(t)
}

/// Earliest usable window start for filtering `/markets/slug` rows.
fn gamma_market_window_start(m: &RawMarket) -> Option<DateTime<Utc>> {
    parse_ts(m.event_start_time.as_deref())
        .or_else(|| parse_ts(m.start_date.as_deref()))
}

fn active_market_from_raw_market(m: &RawMarket, event: Option<&RawEvent>) -> Result<ActiveMarket> {
    let token_ids: Vec<String> = serde_json::from_str(&m.clob_token_ids)
        .context("parsing clobTokenIds")?;
    if token_ids.len() < 2 {
        return Err(anyhow!("expected 2 token ids, got {}", token_ids.len()));
    }
    let outcomes: Vec<String> = serde_json::from_str(&m.outcomes)
        .unwrap_or_else(|_| vec!["Up".into(), "Down".into()]);

    let (u0, d0) = if outcomes
        .first()
        .map(|s| s.eq_ignore_ascii_case("up"))
        .unwrap_or(true)
    {
        (token_ids[0].as_str(), token_ids[1].as_str())
    } else {
        (token_ids[1].as_str(), token_ids[0].as_str())
    };
    let up_token_id = canonical_clob_token_id(u0).into_owned();
    let down_token_id = canonical_clob_token_id(d0).into_owned();

    let price_to_beat = extract_price_to_beat(&m.description); // overridden in find_current_btc_5m when API returns openPrice
    // Canonical tick strings for `ROUNDING_CONFIG` parity (avoid `0.001 → "0.00"` with `:.2`).
    let tick_size = match m.tick_size_f {
        Some(v) if (v - 0.1).abs() < 1e-6 => "0.1".into(),
        Some(v) if (v - 0.01).abs() < 1e-7 => "0.01".into(),
        Some(v) if (v - 0.001).abs() < 1e-8 => "0.001".into(),
        Some(v) if (v - 0.0001).abs() < 1e-9 => "0.0001".into(),
        Some(v) => gamma_tick_f64_to_string(v),
        None => "0.01".into(),
    };

    let opens_at = parse_ts(m.event_start_time.as_deref())
        .or_else(|| parse_ts(m.start_date.as_deref()))
        .or_else(|| event.and_then(|e| parse_ts(e.start_date.as_deref())))
        .unwrap_or_else(Utc::now);
    let closes_at = parse_ts(m.end_date.as_deref())
        .or_else(|| event.and_then(|e| parse_ts(e.end_date.as_deref())))
        .unwrap_or_else(Utc::now);

    Ok(ActiveMarket {
        condition_id: m.condition_id.clone(),
        question:     m.question.clone(),
        slug:         m.slug.clone(),
        up_token_id,
        down_token_id,
        tick_size,
        neg_risk:    m.neg_risk,
        price_to_beat,
        opens_at,
        closes_at,
        crypto_price_query_start_utc: String::new(),
        crypto_price_query_end_utc:   String::new(),
    })
}

impl Default for GammaClient {
    fn default() -> Self { Self::new() }
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// Pull "Price to Beat" out of the market description text. The resolution
/// text typically contains something like:
///
///   "The opening BTC price for this window is $67,234.50."
///
/// If the bot can't parse it, we set `price_to_beat = None` and the header
/// colour logic will fall back to comparing against the market implied
/// probability (YES price vs 0.50).
fn extract_price_to_beat(desc: &str) -> Option<f64> {
    // Prefer text near "price to beat" / "opening" (older Gamma copy).
    let lower = desc.to_ascii_lowercase();
    if let Some(anchor) = lower
        .find("price to beat")
        .or_else(|| lower.find("opening"))
        .or_else(|| lower.find("beginning of that range"))
    {
        let tail = &desc[anchor..];
        if let Some(dollar) = tail.find('$') {
            let rest = &tail[dollar + 1..];
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
                .collect();
            if let Ok(v) = num.replace(',', "").parse::<f64>() {
                if v > 1.0 {
                    return Some(v);
                }
            }
        }
    }
    // Newer events often omit a dollar figure in `description`; if Polymarket
    // adds any "$87,123" style reference, grab the first BTC-plausible value.
    for seg in desc.split('$').skip(1) {
        let num: String = seg
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
            .collect();
        if let Ok(v) = num.replace(',', "").parse::<f64>() {
            if (10_000.0..2_000_000.0).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tick_string_tests {
    use super::gamma_tick_f64_to_string;

    #[test]
    fn formats_tick_without_excess_trailing_zeros() {
        assert_eq!(gamma_tick_f64_to_string(0.001), "0.001");
        assert_eq!(gamma_tick_f64_to_string(0.15), "0.15");
        assert_eq!(gamma_tick_f64_to_string(0.123456789012), "0.123456789012");
    }
}
