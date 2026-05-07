//! Polymarket relayer v2 — `POST /submit` with **`RELAYER_API_KEY`** + **`RELAYER_API_KEY_ADDRESS`**
//! (Settings → API → Relayer), same as CTF redeem in [`crate::redeem`].

use alloy_primitives::Address;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

const SUBMIT_PATH: &str = "/submit";
const GET_TX_PATH: &str = "/transaction";

#[derive(Debug, Deserialize)]
pub struct RelayerSubmitResponse {
    #[serde(rename = "transactionID")]
    pub transaction_id: String,
    pub state: String,
    #[serde(default, rename = "transactionHash")]
    pub transaction_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RelayerTransactionRow {
    pub state: String,
    #[serde(default, rename = "transactionHash")]
    pub transaction_hash: Option<String>,
}

pub(crate) fn normalize_relayer_base(url: &str) -> String {
    let u = url.trim();
    if u.ends_with('/') {
        u[..u.len() - 1].to_string()
    } else {
        u.to_string()
    }
}

#[derive(Debug, Deserialize)]
struct NonceResponse {
    nonce: String,
}

/// `GET /nonce?address=…&type=WALLET` — current nonce for deposit-wallet `WALLET` batches.
pub async fn relayer_wallet_nonce(
    http: &Client,
    relayer_base_url: &str,
    owner: Address,
) -> Result<String> {
    let base = normalize_relayer_base(relayer_base_url);
    let url = format!("{base}/nonce?address={owner:#x}&type=WALLET");
    let resp = http
        .get(&url)
        .send()
        .await
        .context("GET relayer /nonce (WALLET)")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer GET /nonce (WALLET): HTTP {} — {}", status, txt.trim());
    }
    let n: NonceResponse =
        serde_json::from_str(&txt).with_context(|| format!("decode /nonce: {}", txt.trim()))?;
    Ok(n.nonce)
}

/// `POST /submit` with arbitrary JSON body (relayer API key headers).
pub async fn submit_relayer_json(
    http: &Client,
    relayer_base_url: &str,
    relayer_api_key: &str,
    relayer_api_key_address: Address,
    body: &serde_json::Value,
) -> Result<RelayerSubmitResponse> {
    let url = format!("{}{SUBMIT_PATH}", normalize_relayer_base(relayer_base_url));
    let resp = http
        .post(&url)
        .header("RELAYER_API_KEY", relayer_api_key)
        .header(
            "RELAYER_API_KEY_ADDRESS",
            format!("{relayer_api_key_address:#x}"),
        )
        .json(body)
        .send()
        .await
        .context("POST relayer /submit")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer POST /submit: HTTP {} — {}", status, txt.trim());
    }
    serde_json::from_str(&txt).with_context(|| format!("decode /submit JSON: {}", txt.trim()))
}

/// After a `WALLET` batch completes on-chain, Polymarket may still return HTTP 400
/// `wallet busy: active action exists` until the relayer clears its internal lock. Retry with backoff.
pub async fn submit_relayer_json_retry_on_wallet_busy(
    http: &Client,
    relayer_base_url: &str,
    relayer_api_key: &str,
    relayer_api_key_address: Address,
    body: &serde_json::Value,
) -> Result<RelayerSubmitResponse> {
    const MAX_ATTEMPTS: u32 = 18;
    for attempt in 0..MAX_ATTEMPTS {
        match submit_relayer_json(
            http,
            relayer_base_url,
            relayer_api_key,
            relayer_api_key_address,
            body,
        )
        .await
        {
            Ok(r) => return Ok(r),
            Err(e) => {
                let s = format!("{e:#}").to_lowercase();
                let transient = s.contains("wallet busy") || s.contains("active action");
                if transient && attempt + 1 < MAX_ATTEMPTS {
                    let ms = 400u64.saturating_add(u64::from(attempt).saturating_mul(150));
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                return Err(e);
            }
        }
    }
    bail!("relayer POST /submit: exhausted retries (wallet busy)")
}

#[derive(serde::Serialize)]
struct WalletCreateBody {
    #[serde(rename = "type")]
    ty: &'static str,
    from: String,
    to: String,
}

fn wallet_create_body(owner: Address, factory: Address) -> WalletCreateBody {
    WalletCreateBody {
        ty: "WALLET-CREATE",
        from: format!("{owner:#x}"),
        to: format!("{factory:#x}"),
    }
}

/// Submit [`WALLET-CREATE`](https://docs.polymarket.com/trading/deposit-wallet-migration).
pub async fn submit_wallet_create(
    http: &Client,
    relayer_base_url: &str,
    owner: Address,
    factory: Address,
    relayer_api_key: &str,
    relayer_api_key_address: Address,
) -> Result<RelayerSubmitResponse> {
    let body = serde_json::to_value(wallet_create_body(owner, factory)).context("encode WALLET-CREATE")?;
    submit_relayer_json(
        http,
        relayer_base_url,
        relayer_api_key,
        relayer_api_key_address,
        &body,
    )
    .await
}

pub async fn get_transaction_rows(
    http: &Client,
    relayer_base_url: &str,
    transaction_id: &str,
) -> Result<Vec<RelayerTransactionRow>> {
    let base = normalize_relayer_base(relayer_base_url);
    let mut u = url::Url::parse(&format!("{base}{GET_TX_PATH}"))
        .with_context(|| format!("invalid relayer URL {base}"))?;
    u.query_pairs_mut().append_pair("id", transaction_id);
    let resp = http
        .get(u.as_str())
        .send()
        .await
        .context("GET relayer /transaction")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer GET /transaction: HTTP {} — {}", status, txt.trim());
    }
    serde_json::from_str(&txt).with_context(|| format!("decode /transaction: {}", txt.trim()))
}

/// Poll until mined/confirmed or failed.
pub async fn wait_relayer_transaction(
    http: &Client,
    relayer_base_url: &str,
    transaction_id: &str,
    max_polls: u32,
    poll_ms: u64,
) -> Result<RelayerTransactionRow> {
    for _ in 0..max_polls {
        let rows = get_transaction_rows(http, relayer_base_url, transaction_id).await?;
        if let Some(txn) = rows.into_iter().next() {
            match txn.state.as_str() {
                "STATE_MINED" | "STATE_CONFIRMED" => return Ok(txn),
                "STATE_FAILED" | "STATE_INVALID" => {
                    bail!("relayer transaction {} failed (state={})", transaction_id, txn.state);
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
    bail!(
        "relayer transaction {} did not reach STATE_MINED / STATE_CONFIRMED within {} polls",
        transaction_id,
        max_polls
    );
}
