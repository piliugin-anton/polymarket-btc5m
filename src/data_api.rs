//! Polymarket Data API — read-only user positions (`https://data-api.polymarket.com`).
//!
//! Claimable USDC from resolved markets is the sum of `currentValue` on positions with `redeemable: true`.
//! Redeeming on-chain uses CTF `redeemPositions` (see Polymarket CTF docs); typical accounts hold tokens in a
//! Safe — use the web Portfolio **Claim** flow or `polymarket ctf redeem` (official CLI).
//!
//! [`fetch_positions_for_market`] returns **total** outcome size (incl. shares escrowed for resting SELLs),
//! useful when CLOB spendable balance + trade replay are both empty or inconsistent.

use std::collections::{HashMap, HashSet};

use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::trading::clob_asset_ids_match;

pub const DATA_API_HOST: &str = "https://data-api.polymarket.com";

fn body_snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    let max_chars = 500;
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        format!("{}...", trimmed.chars().take(max_chars).collect::<String>())
    }
}

fn decode_json_slice<T: DeserializeOwned>(body: &[u8], label: &str) -> Result<T> {
    serde_json::from_slice(body).with_context(|| format!("decode {label}: {}", body_snippet(body)))
}

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
    #[allow(dead_code)]
    // deserialized from API; redeem uses adapters that read balances on-chain
    pub outcome_index: u32,
    #[serde(rename = "negativeRisk", default)]
    pub negative_risk: bool,
}

const POSITIONS_PAGE: u32 = 500;
/// Data API caps `offset` at 10_000 (`offset` + page must stay within spec).
const POSITIONS_MAX_OFFSET: u32 = 10_000;

/// `(shares, avg_price)` for one UP/DOWN leg from Data API rows.
pub type PositionsTokenSizeAvg = Option<(f64, f64)>;

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
            "{DATA_API_HOST}/positions?user={user:#x}&limit={POSITIONS_PAGE}&offset={offset}&redeemable=true&sizeThreshold=0",
        );
        let resp = http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body = resp.bytes().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "data-api GET /positions failed: {} — {}",
                status,
                body_snippet(&body)
            );
        }
        let batch: Vec<DataPosition> = decode_json_slice(&body, "/positions")?;
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
        "{DATA_API_HOST}/positions?user={user:#x}&market={market_condition_id}&sizeThreshold=0&limit=500",
    );
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.bytes().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "data-api GET /positions (market) failed: {} — {}",
            status,
            body_snippet(&body)
        );
    }
    let batch: Vec<DataPosition> = decode_json_slice(&body, "/positions (market)")?;
    Ok(batch)
}

/// One row from public [`GET /trades`](https://docs.polymarket.com/api-reference/core/get-trades-for-a-user-or-markets) (no auth).
#[derive(Debug, Clone, Deserialize)]
pub struct DataApiPublicTrade {
    #[serde(default)]
    pub asset: String,
    #[serde(default)]
    pub price: f64,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub timestamp: i64,
}

/// `GET /trades?market=<conditionId>&limit=…` — public trade tape for activity bootstrap.
pub async fn fetch_public_trades_for_market(
    http: &reqwest::Client,
    market_condition_id: &str,
    limit: u32,
) -> Result<Vec<DataApiPublicTrade>> {
    let mut u = reqwest::Url::parse(&format!("{DATA_API_HOST}/trades"))
        .context("parse Data API /trades URL")?;
    u.query_pairs_mut()
        .append_pair("market", market_condition_id)
        .append_pair("limit", &limit.to_string());
    let resp = http
        .get(u.as_str())
        .send()
        .await
        .with_context(|| format!("GET {}", u.as_str()))?;
    let status = resp.status();
    let body = resp.bytes().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "data-api GET /trades failed: {} — {}",
            status,
            body_snippet(&body)
        );
    }
    let batch: Vec<DataApiPublicTrade> = decode_json_slice(&body, "/trades")?;
    Ok(batch)
}

/// Map Data API rows to UP/DOWN `(shares, avg_price)` using the same token-id rules as CLOB.
pub fn positions_size_avg_for_tokens(
    rows: &[DataPosition],
    up_token_id: &str,
    down_token_id: &str,
) -> (PositionsTokenSizeAvg, PositionsTokenSizeAvg) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_decode_error_includes_trimmed_body_snippet() {
        let err = decode_json_slice::<Vec<DataPosition>>(b"  not-json-body  ", "/positions")
            .expect_err("invalid JSON should fail");

        assert!(err.to_string().contains("decode /positions: not-json-body"));
    }
}

// ── Top holders (`GET /holders`) — sentiment proxy (sums are capped at API `limit` per outcome) ──

/// Per-outcome holder rows to request (Polymarket allows up to 20; we use 5).
pub const HOLDERS_REQUEST_LIMIT: u32 = 5;

#[derive(Debug, Clone, Deserialize)]
struct MetaHolder {
    token: String,
    holders: Vec<HolderRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct HolderRow {
    #[serde(default, rename = "proxyWallet")]
    proxy_wallet: String,
    #[serde(default)]
    amount: f64,
}

/// `GET /holders?...&limit=[`HOLDERS_REQUEST_LIMIT`]` — sentiment sums with **per-wallet** netting:
/// if a `proxyWallet` has both UP and DOWN positions, only the **larger** `amount` counts (toward that side).
/// Rows with missing/empty `proxyWallet` are skipped.
pub async fn fetch_top_holders_amount_sums(
    http: &reqwest::Client,
    market_condition_id: &str,
    up_token_id: &str,
    down_token_id: &str,
) -> Result<(f64, f64)> {
    const WALLET_EPS: f64 = 1e-9;

    let url = format!(
        "{DATA_API_HOST}/holders?market={}&limit={}",
        market_condition_id, HOLDERS_REQUEST_LIMIT
    );
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.bytes().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "data-api GET /holders failed: {} — {}",
            status,
            body_snippet(&body)
        );
    }
    let meta: Vec<MetaHolder> = decode_json_slice(&body, "/holders")?;

    let mut by_wallet_up: HashMap<String, f64> = HashMap::new();
    let mut by_wallet_down: HashMap<String, f64> = HashMap::new();
    for block in &meta {
        let is_up = clob_asset_ids_match(&block.token, up_token_id);
        let is_down = if is_up {
            false
        } else {
            clob_asset_ids_match(&block.token, down_token_id)
        };
        if !is_up && !is_down {
            continue;
        }
        let side = if is_up {
            &mut by_wallet_up
        } else {
            &mut by_wallet_down
        };
        for h in &block.holders {
            if !h.amount.is_finite() || h.amount < 0.0 {
                continue;
            }
            let w = norm_proxy_wallet_id(&h.proxy_wallet);
            if w.is_empty() {
                continue;
            }
            *side.entry(w).or_insert(0.0) += h.amount;
        }
    }

    let keys: HashSet<String> = by_wallet_up
        .keys()
        .chain(by_wallet_down.keys())
        .cloned()
        .collect();
    let mut up_sum = 0.0f64;
    let mut down_sum = 0.0f64;
    for w in keys {
        let u = *by_wallet_up.get(&w).unwrap_or(&0.0);
        let d = *by_wallet_down.get(&w).unwrap_or(&0.0);
        if u < WALLET_EPS && d < WALLET_EPS {
            continue;
        }
        if d < WALLET_EPS {
            up_sum += u;
        } else if u < WALLET_EPS {
            down_sum += d;
        } else if (u - d).abs() <= WALLET_EPS {
            // Equal UP/DOWN: no unique "higher" leg — omit both from sentiment sums.
        } else if u > d {
            up_sum += u;
        } else {
            down_sum += d;
        }
    }
    Ok((up_sum, down_sum))
}

fn norm_proxy_wallet_id(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}
