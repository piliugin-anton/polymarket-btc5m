//! Polymarket Data API — read-only user positions (`https://data-api.polymarket.com`).
//!
//! Claimable USDC from resolved markets is the sum of `currentValue` on positions with `redeemable: true`.
//! Redeeming on-chain uses CTF `redeemPositions` (see Polymarket CTF docs); typical accounts hold tokens in a
//! Safe — use the web Portfolio **Claim** flow or `polymarket ctf redeem` (official CLI).
//!
//! [`fetch_positions_for_market`] returns **total** outcome size (incl. shares escrowed for resting SELLs),
//! useful when CLOB spendable balance + trade replay are both empty or inconsistent.

use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

use crate::trading::clob_asset_ids_match;

pub const DATA_API_HOST: &str = "https://data-api.polymarket.com";

#[derive(Debug, Clone, Deserialize)]
pub struct DataPosition {
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    #[serde(default)]
    pub redeemable: bool,
    /// Approximate USDC value of this position in the API (use for claimable estimate).
    #[serde(rename = "currentValue")]
    pub current_value: f64,
    #[serde(default)]
    #[allow(dead_code)]
    pub title: String,
    /// Outcome ERC-1155 token id (decimal or 0x hex).
    #[serde(default)]
    pub asset: String,
    /// Position size in outcome shares (Data API).
    #[serde(default)]
    pub size: f64,
    /// Volume-average entry price from Data API (USDC per share).
    #[serde(default, rename = "avgPrice")]
    pub avg_price: f64,
    #[serde(rename = "outcomeIndex", default)]
    pub outcome_index: u32,
    #[serde(rename = "negativeRisk", default)]
    pub negative_risk: bool,
}

const POSITIONS_PAGE: u32 = 500;
/// Data API caps `offset` at 10_000 (`offset` + page must stay within spec).
const POSITIONS_MAX_OFFSET: u32 = 10_000;

/// `GET /positions?user=0x...&redeemable=true` with pagination.
///
/// Without `redeemable=true`, the default sort (`TOKENS` DESC) returns only the first `limit`
/// positions — redeemable (often small) rows can fall **past** that window, so claimable sums to 0
/// while the site still shows claimable (it queries redeemable positions).
pub async fn fetch_redeemable_positions(
    http: &reqwest::Client,
    user: Address,
) -> Result<Vec<DataPosition>> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    loop {
        // API default `sizeThreshold` is 1; omitting it drops positions with size < 1 (site/scripts use 0).
        let url = format!(
            "{DATA_API_HOST}/positions?user={}&limit={POSITIONS_PAGE}&offset={offset}&redeemable=true&sizeThreshold=0",
            format!("{user:#x}")
        );
        let resp = http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("data-api GET /positions failed: {} — {}", status, txt.trim());
        }
        let batch: Vec<DataPosition> = serde_json::from_str(&txt)
            .with_context(|| format!("decode /positions: {}", txt.trim()))?;
        let n = batch.len();
        out.extend(batch);
        if n < POSITIONS_PAGE as usize {
            break;
        }
        offset = offset.saturating_add(POSITIONS_PAGE);
        if offset > POSITIONS_MAX_OFFSET {
            break;
        }
    }
    Ok(out)
}

#[allow(dead_code)] // Legacy Data-API-only claimable sum; balance panel uses on-chain totals in `balances`.
pub fn sum_claimable_usdc(positions: &[DataPosition]) -> f64 {
    positions
        .iter()
        .filter(|p| p.redeemable)
        .map(|p| p.current_value)
        .filter(|v| v.is_finite())
        .sum()
}

/// `GET /positions?user=…&market=<conditionId>&sizeThreshold=0` — rows for one market only.
pub async fn fetch_positions_for_market(
    http: &reqwest::Client,
    user: Address,
    market_condition_id: &str,
) -> Result<Vec<DataPosition>> {
    let url = format!(
        "{DATA_API_HOST}/positions?user={}&market={}&sizeThreshold=0&limit=500",
        format!("{user:#x}"),
        market_condition_id
    );
    let resp = http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("data-api GET /positions (market) failed: {} — {}", status, txt.trim());
    }
    let batch: Vec<DataPosition> = serde_json::from_str(&txt)
        .with_context(|| format!("decode /positions (market): {}", txt.trim()))?;
    Ok(batch)
}

/// Map Data API rows to UP/DOWN `(shares, avg_price)` using the same token-id rules as CLOB.
pub fn positions_size_avg_for_tokens(
    rows: &[DataPosition],
    up_token_id: &str,
    down_token_id: &str,
) -> (Option<(f64, f64)>, Option<(f64, f64)>) {
    let mut up: Option<(f64, f64)> = None;
    let mut down: Option<(f64, f64)> = None;
    for p in rows {
        if !p.size.is_finite() || p.size <= 1e-12 {
            continue;
        }
        let avg = if p.avg_price.is_finite() && p.avg_price > 0.0 {
            p.avg_price
        } else {
            0.0
        };
        let pair = (p.size, avg);
        if clob_asset_ids_match(&p.asset, up_token_id) {
            up = Some(pair);
        } else if clob_asset_ids_match(&p.asset, down_token_id) {
            down = Some(pair);
        }
    }
    (up, down)
}

// ── Top holders (`GET /holders`) — sentiment proxy (sums are capped at API `limit` per outcome) ──

#[derive(Debug, Clone, Deserialize)]
struct MetaHolder {
    token:   String,
    holders: Vec<HolderRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct HolderRow {
    #[serde(default)]
    amount: f64,
}

/// `GET /holders?market=<conditionId>&limit=20` — sums [`HolderRow::amount`] per outcome token.
///
/// Polymarket caps `limit` at 20 per token; totals are **top-holder** sums, not full open interest.
pub async fn fetch_top_holders_amount_sums(
    http: &reqwest::Client,
    market_condition_id: &str,
    up_token_id: &str,
    down_token_id: &str,
) -> Result<(f64, f64)> {
    let url = format!(
        "{DATA_API_HOST}/holders?market={}&limit=20",
        market_condition_id
    );
    let resp = http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("data-api GET /holders failed: {} — {}", status, txt.trim());
    }
    let meta: Vec<MetaHolder> =
        serde_json::from_str(&txt).with_context(|| format!("decode /holders: {}", txt.trim()))?;
    let mut up_sum = 0.0f64;
    let mut down_sum = 0.0f64;
    for block in meta {
        let chunk: f64 = block
            .holders
            .iter()
            .map(|h| h.amount)
            .filter(|a| a.is_finite())
            .sum();
        if clob_asset_ids_match(&block.token, up_token_id) {
            up_sum += chunk;
        } else if clob_asset_ids_match(&block.token, down_token_id) {
            down_sum += chunk;
        }
    }
    Ok((up_sum, down_sum))
}
