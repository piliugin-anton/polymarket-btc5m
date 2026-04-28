//! On-chain balance panel: trading cash + claimable CTF payout (standard markets).
//!
//! * **Cash** — `USDC.e.balanceOf(funder)` **plus** [pUSD](https://docs.polymarket.com/resources/contracts)
//!   (`CollateralToken` proxy). After CLOB V2 (Apr 2026), Polymarket collateral is pUSD; funds can
//!   show as zero USDC.e while pUSD holds the spendable balance ([migration](https://docs.polymarket.com/v2-migration)).
//! * **Claimable** — standard redeemable rows: `eth_call` on CTF using payout vectors plus
//!   `balanceOf(funder, asset)` where `asset` is the outcome token id from the Data API (same id
//!   the CLOB uses). Rows whose token id does not match derived CTF positions fall back to
//!   `currentValue`. Neg-risk rows use Data API `currentValue`. The panel uses
//!   `max(standard_on_chain + neg_api, sum_api_claimable)` so mis-tagged neg-risk or indexing gaps
//!   do not show as zero when the API still reports redeemable value.
//!
//! Reads use Polygon [**Multicall3**](https://github.com/mds1/multicall) `aggregate3` at
//! `0xcA11…CA11`: one JSON-RPC `eth_call` packs USDC `balanceOf`, all CTF payout reads, and all
//! CTF `balanceOf` outcome tokens (chunked if the call list exceeds [`MAX_AGGREGATE3_CALLS`]).
//! Sub-calls use `allowFailure=true` so one bad slot does not revert the whole batch.
//!
//! Polygon JSON-RPC uses a **direct** HTTP client (no `POLYMARKET_PROXY`); Data API keeps using the
//! proxy-aware client from [`crate::net`].

use std::collections::{HashMap, HashSet};

use alloy_primitives::{address, keccak256, Address, B256, U256};
use alloy_sol_types::{sol, SolCall};
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;

use crate::data_api::{self, DataPosition};
use crate::redeem::{parse_condition_id, parse_token_id_u256};

const USDC_E: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
/// Polymarket USD (pUSD) — CLOB V2 collateral ([contracts](https://docs.polymarket.com/resources/contracts)).
const PUSD: Address = address!("0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB");
const CTF: Address = address!("0x4D97DCd97eC945f40cF65f87097ACe5EA0476045");
/// [Multicall3](https://github.com/mds1/multicall) on Polygon (same address on most EVM chains).
const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

/// Max sub-calls per `aggregate3` (stay within RPC `eth_call` input limits).
const MAX_AGGREGATE3_CALLS: usize = 256;

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

sol! {
    interface ICTF {
        function payoutDenominator(bytes32 conditionId) external view returns (uint256);
        function payoutNumerators(bytes32 conditionId, uint256 index) external view returns (uint256);
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

sol! {
    interface Multicall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Aggregate3Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls)
            external
            payable
            returns (Aggregate3Result[] memory returnData);
    }
}

/// HTTP client for Polygon RPC only — does not use `POLYMARKET_PROXY`.
///
/// Same HTTP/2 + keep-alive pooled connection as [`crate::net::reqwest_client`]; see
/// [`crate::net::polygon_rpc_reqwest_client`].
pub fn polygon_rpc_http_client() -> Result<Client> {
    crate::net::polygon_rpc_reqwest_client()
}

fn parent_collection_id() -> B256 {
    B256::ZERO
}

/// GCTF `getCollectionId` / `getPositionId` (same as Polymarket CTF layout for binary markets).
fn collection_id(condition_id: B256, index_set: u64) -> B256 {
    let mut buf = Vec::with_capacity(96);
    buf.extend_from_slice(parent_collection_id().as_slice());
    buf.extend_from_slice(condition_id.as_slice());
    buf.extend_from_slice(&U256::from(index_set).to_be_bytes::<32>());
    keccak256(buf)
}

fn position_id(collateral: Address, collection_id: B256) -> U256 {
    let mut buf = Vec::with_capacity(52);
    buf.extend_from_slice(collateral.as_slice());
    buf.extend_from_slice(collection_id.as_slice());
    let h = keccak256(buf);
    U256::from_be_slice(h.as_slice())
}

fn u256_to_usdc_f64(v: U256) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(0.0) / 1_000_000.0
}

fn decode_uint256_return(data: &[u8]) -> Result<U256> {
    if data.len() < 32 {
        anyhow::bail!("returnData too short: {}", data.len());
    }
    Ok(U256::from_be_slice(&data[data.len() - 32..]))
}

async fn rpc_eth_call(
    http: &Client,
    rpc_url: &str,
    to: Address,
    calldata: &[u8],
) -> Result<Vec<u8>> {
    let data_hex = format!(
        "0x{}",
        calldata.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let body = json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": format!("{to:#x}"), "data": data_hex}, "latest"],
        "id": 1
    });
    let resp = http
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("RPC eth_call")?;
    let v: serde_json::Value = resp.json().await.context("RPC JSON")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("RPC error: {err}");
    }
    let s = v
        .get("result")
        .and_then(|x| x.as_str())
        .context("RPC missing result")?;
    hex::decode(s.trim_start_matches("0x")).context("decode eth_call hex")
}

/// Multicall3 `aggregate3`: one `eth_call`, `allowFailure` per slot. Chunks to respect calldata limits.
async fn rpc_aggregate3_calls(
    http: &Client,
    rpc_url: &str,
    items: &[(Address, bool, Vec<u8>)],
) -> Result<Vec<(bool, Vec<u8>)>> {
    if items.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::with_capacity(items.len());
    for chunk in items.chunks(MAX_AGGREGATE3_CALLS) {
        let calls: Vec<Multicall3::Call3> = chunk
            .iter()
            .map(|(target, allow_failure, data)| Multicall3::Call3 {
                target: *target,
                allowFailure: *allow_failure,
                callData: data.clone().into(),
            })
            .collect();
        let calldata = Multicall3::aggregate3Call { calls }.abi_encode();
        let raw = rpc_eth_call(http, rpc_url, MULTICALL3, &calldata).await?;
        let decoded = Multicall3::aggregate3Call::abi_decode_returns(&raw, false)
            .context("Multicall3 aggregate3 decode")?;
        let rows = decoded.returnData;
        let n = rows.len();
        if n != chunk.len() {
            anyhow::bail!(
                "aggregate3: got {n} results for {} calls",
                chunk.len()
            );
        }
        for r in rows {
            out.push((r.success, r.returnData.to_vec()));
        }
    }
    Ok(out)
}

struct StandardClaimPrep {
    parsed: Vec<(B256, U256, f64)>,
    conds: Vec<B256>,
    token_ids: Vec<U256>,
}

fn prepare_standard_claimable_parsed(rows: &[DataPosition]) -> StandardClaimPrep {
    let mut by_key: HashMap<(B256, U256), f64> = HashMap::new();
    for p in rows {
        if !p.redeemable || p.negative_risk {
            continue;
        }
        let Ok(cid) = parse_condition_id(&p.condition_id) else {
            continue;
        };
        let Ok(tid) = parse_token_id_u256(&p.asset) else {
            continue;
        };
        if !p.current_value.is_finite() {
            continue;
        }
        by_key
            .entry((cid, tid))
            .and_modify(|v| *v = v.max(p.current_value))
            .or_insert(p.current_value);
    }
    let parsed: Vec<(B256, U256, f64)> = by_key.into_iter().map(|((c, t), v)| (c, t, v)).collect();
    if parsed.is_empty() {
        return StandardClaimPrep {
            parsed,
            conds: vec![],
            token_ids: vec![],
        };
    }
    let mut conds: Vec<B256> = parsed.iter().map(|(c, _, _)| *c).collect::<HashSet<_>>().into_iter().collect();
    conds.sort_by(|a, b| a.as_slice().cmp(b.as_slice()));
    let mut token_ids: Vec<U256> = parsed.iter().map(|(_, t, _)| *t).collect::<HashSet<_>>().into_iter().collect();
    token_ids.sort();
    StandardClaimPrep {
        parsed,
        conds,
        token_ids,
    }
}

fn claimable_standard_total_from_maps(
    parsed: &[(B256, U256, f64)],
    payouts: &HashMap<B256, (U256, U256, U256)>,
    balances: &HashMap<U256, U256>,
) -> f64 {
    let mut total = 0.0f64;
    for &(cid, tid, api_fb) in parsed {
        let Some((d, n0, n1)) = payouts.get(&cid).copied() else {
            total += api_fb;
            continue;
        };
        let pos1_usdc = position_id(USDC_E, collection_id(cid, 1));
        let pos2_usdc = position_id(USDC_E, collection_id(cid, 2));
        let pos1_pusd = position_id(PUSD, collection_id(cid, 1));
        let pos2_pusd = position_id(PUSD, collection_id(cid, 2));
        let b = balances.get(&tid).copied().unwrap_or(U256::ZERO);

        if d.is_zero() {
            total += api_fb;
            continue;
        }
        if tid != pos1_usdc && tid != pos2_usdc && tid != pos1_pusd && tid != pos2_pusd {
            total += api_fb;
            continue;
        }
        let n = if tid == pos1_usdc || tid == pos1_pusd { n0 } else { n1 };
        let slot_usdc = u256_to_usdc_f64(b * n / d);
        if slot_usdc > 1e-9 {
            total += slot_usdc;
        } else if api_fb > 1e-9 {
            total += api_fb;
        }
    }
    total
}

async fn fetch_onchain_cash_and_claim_std(
    http_rpc: &Client,
    rpc_url: &str,
    funder: Address,
    redeemable_rows: &[DataPosition],
) -> Result<(f64, f64)> {
    let prep = prepare_standard_claimable_parsed(redeemable_rows);
    if prep.parsed.is_empty() {
        let cash = collateral_cash_usdc_f64(http_rpc, rpc_url, funder).await?;
        return Ok((cash, 0.0));
    }

    let StandardClaimPrep {
        parsed,
        conds,
        token_ids,
    } = prep;

    let mut calls: Vec<(Address, bool, Vec<u8>)> = Vec::new();
    calls.push((
        USDC_E,
        true,
        IERC20::balanceOfCall { account: funder }.abi_encode(),
    ));
    calls.push((
        PUSD,
        true,
        IERC20::balanceOfCall { account: funder }.abi_encode(),
    ));
    for cond in &conds {
        calls.push((
            CTF,
            true,
            ICTF::payoutDenominatorCall { conditionId: *cond }.abi_encode(),
        ));
        calls.push((
            CTF,
            true,
            ICTF::payoutNumeratorsCall {
                conditionId: *cond,
                index: U256::ZERO,
            }
            .abi_encode(),
        ));
        calls.push((
            CTF,
            true,
            ICTF::payoutNumeratorsCall {
                conditionId: *cond,
                index: U256::from(1u64),
            }
            .abi_encode(),
        ));
    }
    for tid in &token_ids {
        calls.push((
            CTF,
            true,
            ICTF::balanceOfCall {
                account: funder,
                id: *tid,
            }
            .abi_encode(),
        ));
    }

    let results = rpc_aggregate3_calls(http_rpc, rpc_url, &calls).await?;

    let mut i = 0usize;
    let (usdc_ok, usdc_raw) = &results[i];
    i += 1;
    let (pusd_ok, pusd_raw) = &results[i];
    i += 1;
    let usdc_amt = if *usdc_ok {
        decode_uint256_return(usdc_raw)
            .map(u256_to_usdc_f64)
            .unwrap_or(0.0)
    } else {
        0.0
    };
    let pusd_amt = if *pusd_ok {
        decode_uint256_return(pusd_raw)
            .map(u256_to_usdc_f64)
            .unwrap_or(0.0)
    } else {
        0.0
    };
    let cash = usdc_amt + pusd_amt;

    let mut payouts: HashMap<B256, (U256, U256, U256)> = HashMap::with_capacity(conds.len());
    for cond in &conds {
        let d = results
            .get(i)
            .filter(|(ok, _)| *ok)
            .and_then(|(_, data)| decode_uint256_return(data).ok())
            .unwrap_or(U256::ZERO);
        i += 1;
        let n0 = results
            .get(i)
            .filter(|(ok, _)| *ok)
            .and_then(|(_, data)| decode_uint256_return(data).ok())
            .unwrap_or(U256::ZERO);
        i += 1;
        let n1 = results
            .get(i)
            .filter(|(ok, _)| *ok)
            .and_then(|(_, data)| decode_uint256_return(data).ok())
            .unwrap_or(U256::ZERO);
        i += 1;
        payouts.insert(*cond, (d, n0, n1));
    }

    let mut balances: HashMap<U256, U256> = HashMap::with_capacity(token_ids.len());
    for tid in &token_ids {
        let b = results
            .get(i)
            .filter(|(ok, _)| *ok)
            .and_then(|(_, data)| decode_uint256_return(data).ok())
            .unwrap_or(U256::ZERO);
        i += 1;
        balances.insert(*tid, b);
    }

    debug_assert_eq!(i, results.len());

    let claim_std = claimable_standard_total_from_maps(&parsed, &payouts, &balances);
    Ok((cash, claim_std))
}

async fn erc20_balance_usdc_f64(
    http: &Client,
    rpc_url: &str,
    token: Address,
    funder: Address,
) -> Result<f64> {
    let calldata = IERC20::balanceOfCall { account: funder }.abi_encode();
    let raw = rpc_eth_call(http, rpc_url, token, &calldata).await?;
    let v = decode_uint256_return(&raw)?;
    Ok(u256_to_usdc_f64(v))
}

/// Spendable trading cash: bridged USDC.e plus pUSD (post–CLOB V2 collateral).
async fn collateral_cash_usdc_f64(http: &Client, rpc_url: &str, funder: Address) -> Result<f64> {
    let usdc = erc20_balance_usdc_f64(http, rpc_url, USDC_E, funder).await?;
    let pusd = erc20_balance_usdc_f64(http, rpc_url, PUSD, funder).await?;
    Ok(usdc + pusd)
}

fn sum_neg_risk_claimable_usdc(rows: &[DataPosition]) -> f64 {
    rows.iter()
        .filter(|p| p.redeemable && p.negative_risk)
        .map(|p| p.current_value)
        .filter(|v| v.is_finite())
        .sum()
}

/// Cash (USDC.e) + claimable: standard CTF via Multicall3 `aggregate3`; neg-risk from Data API; floor vs full API sum.
pub async fn fetch_balance_panel_usdc(
    http_data_api: &Client,
    http_rpc: &Client,
    rpc_url: &str,
    funder: Address,
) -> Result<(f64, f64)> {
    let redeemable_rows = match data_api::fetch_redeemable_positions(http_data_api, funder).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "data-api redeemable positions failed — claimable may be understated");
            Vec::new()
        }
    };

    let claim_neg = sum_neg_risk_claimable_usdc(&redeemable_rows);

    let (cash, claim_std) =
        fetch_onchain_cash_and_claim_std(http_rpc, rpc_url, funder, &redeemable_rows).await?;
    let api_total = data_api::sum_claimable_usdc(&redeemable_rows);
    let total_claim = (claim_std + claim_neg).max(api_total);
    Ok((cash, total_claim))
}
