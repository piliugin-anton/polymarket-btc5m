//! Gamma API client.
//!
//! Used exclusively for market *discovery*: the app’s Gamma poll task calls into
//! this no-auth REST API on a **configurable in-window interval** (see
//! `GAMMA_POLL_IN_WINDOW_SECS` in [`crate::feeds::market_discovery_gamma`]); **after** `closes_at` the
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
use url::Url;

use crate::config::{GAMMA_HOST, POLYMARKET_CRYPTO_PRICE_URL};

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

/// Signed CLOB `expiration` (UTC unix seconds) for a **GTD** limit that should stop resting **1
/// second before** the 5m window ends (`window_end`).
///
/// Polymarket applies a **+60s security offset** on GTD: resting ends around `expiration - 60`.
/// So we use `(window_end - 1s) + 60` as the signed field. See Polymarket “Create Order” → GTD.
pub fn clob_gtd_expiration_secs_one_s_before_window_end(window_end: DateTime<Utc>) -> Result<u64> {
    let target = window_end - Duration::seconds(1);
    let now = Utc::now();
    if target <= now {
        bail!("too close to window end for a GTD limit (need the window to close >1s from now)");
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

    /// Find the 5-min BTC market whose trading window contains `now`.
    ///
    /// Strategy: align `now` to the 5-minute grid, then request
    /// `/markets/slug/btc-updown-5m-{unix}` for that window and neighbors.
    /// Pick the market where `start <= now < end`, else the next upcoming window.
    pub async fn find_current_btc_5m(&self) -> Result<ActiveMarket> {
        let now = Utc::now();
        let now_ts = now.timestamp();
        let base = (now_ts / BTC_5M_WINDOW_SEC) * BTC_5M_WINDOW_SEC;

        let mut candidates: Vec<RawMarket> = Vec::new();
        for off in [
            0,
            BTC_5M_WINDOW_SEC,
            -BTC_5M_WINDOW_SEC,
            2 * BTC_5M_WINDOW_SEC,
            -2 * BTC_5M_WINDOW_SEC,
        ] {
            let ts = base + off;
            let slug = format!("btc-updown-5m-{ts}");
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

        candidates.sort_by_key(|m| {
            m.slug
                .strip_prefix("btc-updown-5m-")
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
                    gamma_market_window_start(m).map_or(false, |s| s >= now)
                })
            })
            .ok_or_else(|| {
                anyhow!(
                    "no active or upcoming btc-updown-5m-* market (tried windows around unix {base})"
                )
            })?;

        self.active_market_from_raw_with_crypto(market).await
    }

    /// `GET /markets/slug/btc-updown-5m-{window_start_ts}` only — for fast roll polling.
    ///
    /// Returns `Ok(None)` on 404 or if Gamma marks the market closed (not ready yet).
    pub async fn try_fetch_btc_5m_by_window_start_ts(
        &self,
        window_start_ts: i64,
    ) -> Result<Option<ActiveMarket>> {
        let slug = format!("btc-updown-5m-{window_start_ts}");
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

        self.active_market_from_raw_with_crypto(&m).await.map(Some)
    }

    async fn active_market_from_raw_with_crypto(&self, market: &RawMarket) -> Result<ActiveMarket> {
        let mut m = active_market_from_raw_market(market, None)?;
        let (win_start, win_end) = utc_five_minute_window_bounds(&m.slug, m.opens_at);
        m.opens_at = win_start;
        m.closes_at = win_end;
        let event_start = format_crypto_price_query_time(win_start);
        let end_date = format_crypto_price_query_time(win_end);
        m.crypto_price_query_start_utc = event_start.clone();
        m.crypto_price_query_end_utc = end_date.clone();
        if let Some(open) = self
            .fetch_polymarket_btc_open_price(&event_start, &end_date)
            .await
        {
            m.price_to_beat = Some(open);
        }
        Ok(m)
    }

    /// `GET /api/crypto/crypto-price` — `openPrice` for this 5-minute window.
    async fn fetch_polymarket_btc_open_price(
        &self,
        event_start: &str,
        end_date: &str,
    ) -> Option<f64> {
        let mut u = Url::parse(POLYMARKET_CRYPTO_PRICE_URL).expect("POLYMARKET_CRYPTO_PRICE_URL");
        u.query_pairs_mut()
            .append_pair("symbol", "BTC")
            .append_pair("eventStartTime", event_start)
            .append_pair("variant", "fiveminute")
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

/// 5m BTC windows start on the UTC clock at :00, :05, …, :55 (epoch slice of 300s).
///
/// Prefer the **`btc-updown-5m-{unix}`** slug (authoritative window start); otherwise
/// floor Gamma `opens_at` down to a 300s boundary. End is always start + 5 minutes.
fn utc_five_minute_window_bounds(slug: &str, opens_at: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    const STEP: i64 = BTC_5M_WINDOW_SEC;
    let start = if let Some(tail) = slug.strip_prefix("btc-updown-5m-") {
        if let Ok(ts) = tail.parse::<i64>() {
            DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or(opens_at)
        } else {
            floor_utc_to_five_minutes(opens_at)
        }
    } else {
        floor_utc_to_five_minutes(opens_at)
    };
    let end = start + Duration::seconds(STEP);
    (start, end)
}

fn floor_utc_to_five_minutes(t: DateTime<Utc>) -> DateTime<Utc> {
    let floored = (t.timestamp() / BTC_5M_WINDOW_SEC) * BTC_5M_WINDOW_SEC;
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

    let (up_token_id, down_token_id) = if outcomes
        .first()
        .map(|s| s.eq_ignore_ascii_case("up"))
        .unwrap_or(true)
    {
        (token_ids[0].clone(), token_ids[1].clone())
    } else {
        (token_ids[1].clone(), token_ids[0].clone())
    };

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
