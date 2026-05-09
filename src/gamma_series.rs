//! Gamma `GET /series` — list crypto up/down **assets** for the wizard (lightweight, `exclude_events=true`).

use anyhow::{Context, Result};
use futures_util::future::join_all;
use serde::Deserialize;

use crate::config::GAMMA_HOST;
use crate::market_profile::{CryptoAsset, CRYPTO_ASSETS};

#[derive(Debug, Clone)]
pub struct SeriesRow {
    pub asset: CryptoAsset,
    pub title: String,
    pub volume_24h: Option<f64>,
    /// Gamma `slug` for the 5m series (for debugging / future use).
    #[allow(dead_code)]
    pub series_slug: String,
}

#[derive(Debug, Deserialize)]
struct RawSeries {
    #[serde(default)]
    title: String,
    #[serde(default)]
    slug: String,
    #[serde(default, rename = "volume24hr")]
    volume: Option<f64>,
}

/// When Gamma is unreachable, still show one row per known asset.
pub fn static_fallback_rows() -> Vec<SeriesRow> {
    CRYPTO_ASSETS
        .iter()
        .map(|a| SeriesRow {
            asset: a.clone(),
            title: format!("{} Up or Down", a.label),
            volume_24h: None,
            series_slug: a.series_slug_5m.to_string(),
        })
        .collect()
}

/// Fetch metadata for each known [`CRYPTO_ASSETS`] 5m series.
pub async fn fetch_crypto_series_for_wizard(http: &reqwest::Client) -> Result<Vec<SeriesRow>> {
    let rows = join_all(CRYPTO_ASSETS.iter().map(|a| fetch_series_row(http, a))).await;
    rows.into_iter().collect()
}

fn fallback_row(a: &CryptoAsset) -> SeriesRow {
    SeriesRow {
        asset: a.clone(),
        title: format!("{} Up or Down", a.label),
        volume_24h: None,
        series_slug: a.series_slug_5m.to_string(),
    }
}

async fn fetch_series_row(http: &reqwest::Client, a: &CryptoAsset) -> Result<SeriesRow> {
    let url = format!(
        "{GAMMA_HOST}/series?slug={}&limit=1&exclude_events=true",
        a.series_slug_5m
    );
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Ok(fallback_row(a));
    }
    let list: Vec<RawSeries> = resp.json().await.with_context(|| format!("decode {url}"))?;
    if let Some(s) = list.into_iter().next() {
        Ok(SeriesRow {
            asset: a.clone(),
            title: if s.title.is_empty() {
                a.label.to_string()
            } else {
                s.title
            },
            volume_24h: s.volume,
            series_slug: s.slug,
        })
    } else {
        Ok(fallback_row(a))
    }
}
