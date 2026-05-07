//! CTF redemption via Polymarket Relayer.
//!
//! * **`POLYMARKET_SIG_TYPE=2` (Gnosis Safe)** — gasless Safe `execTransaction` (`type`: `SAFE`).
//! * **`POLYMARKET_SIG_TYPE=3` ([deposit wallet](https://docs.polymarket.com/trading/deposit-wallet-migration))**
//!   — relayer `WALLET` batch with EIP-712 `DepositWallet` / `Batch` (same wire format as approvals).
//!
//! After [CLOB V2 / pUSD](https://docs.polymarket.com/v2-migration), resolved shares live under
//! [`ctf-exchange-v2`](https://github.com/Polymarket/ctf-exchange-v2) **collateral adapters**:
//! [`CtfCollateralAdapter`](https://docs.polymarket.com/resources/contracts) and
//! [`NegRiskCtfCollateralAdapter`](https://docs.polymarket.com/resources/contracts) expose the same
//! `redeemPositions(address,bytes32,bytes32,uint256[])` entrypoint (first args unused). They pull CTF
//! ERC1155, call CTF internally with **USDC.e** as `collateralToken`, then wrap proceeds to **pUSD**
//! (PMCT) for `msg.sender`. Use the **current** collateral adapter addresses from
//! [Contracts / Collateral](https://docs.polymarket.com/resources/contracts); the relayer rejects
//! calls to deprecated adapter deployments.
//!
//! Flow matches [`@polymarket/builder-relayer-client`](https://github.com/Polymarket/builder-relayer-client)
//! (`buildSafeTransactionRequest`): EIP-712 `SafeTx` hash → sign → `POST /submit`.
//! Docs: <https://docs.polymarket.com/developers/builders/relayer-client>,
//! <https://docs.polymarket.com/api-reference/relayer/submit-a-transaction>.

use alloy_dyn_abi::eip712::TypedData;
use alloy_primitives::{address, b256, keccak256, Address, B256, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolCall};
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::{error, warn};

use crate::config::{Config, SignatureType};
use crate::data_api::DataPosition;
use crate::deposit_wallet::DEPOSIT_WALLET_FACTORY_POLYGON;
use crate::deposit_wallet_approvals::{
    deposit_wallet_batch_typed_data, hex0x, relayer_base_url,
    submit_collateral_adapter_allowances_for_adapters,
};
use crate::polymarket_relayer::{
    normalize_relayer_base, relayer_wallet_nonce, submit_relayer_json_retry_on_wallet_busy,
};

const RELAYER_HOST: &str = "https://relayer-v2.polymarket.com";
/// CtfCollateralAdapter (current Polygon deployment — relayer rejects legacy `0x…09718`).
/// [Contracts / Collateral](https://docs.polymarket.com/resources/contracts)
const CTF_COLLATERAL_ADAPTER: Address =
    address!("0xAdA100Db00Ca00073811820692005400218FcE1f");
/// NegRiskCtfCollateralAdapter (current Polygon deployment).
const NEG_RISK_CTF_COLLATERAL_ADAPTER: Address =
    address!("0xadA2005600Dec949baf300f4C6120000bDB6eAab");
/// Gnosis Safe Factory. [Contracts / Wallet factory](https://docs.polymarket.com/resources/contracts#wallet-factory-contracts)
const SAFE_FACTORY: Address = address!("0xaacFeEa03eb1561C4e67d661e40682Bd20E3541b");
const SAFE_INIT_CODE_HASH: B256 =
    b256!("0x2bce2127ff07fb632d16c8347c4ebf501f4841168bed00d9e6ef715ddb6fcecf");
/// Gnosis `MultiSend` — not listed on Contracts; matches `@polymarket/builder-relayer-client` `getContractConfig(137)`.
const SAFE_MULTISEND: Address = address!("0xA238CBeb142c10Ef7Ad8442C6D1f9E89e07e7761");
/// Max adapter calls per Safe `execTransaction`. One huge MultiSend often hits relayer gas/sim limits (`STATE_FAILED`);
/// smaller batches stay under the limit while **each** batch still uses MultiSend when it contains 2+ redeems.
const REDEEM_MULTISEND_CHUNK: usize = 8;

sol! {
    /// Same selector/ABI as CTF `redeemPositions`. Adapter still routes internally with USDC.e / pUSD;
    /// `collateralToken` / `parentCollectionId` are unused on-chain. Use binary index sets `[1, 2]` per
    /// [Redeem Tokens](https://docs.polymarket.com/trading/ctf/redeem).
    contract CtfCollateralAdapter {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] indexSets
        ) external;
    }
}

sol! {
    contract MultiSend {
        function multiSend(bytes transactions) external;
    }
}

sol! {
    interface IERC1155Ops {
        function isApprovedForAll(address account, address operator) external view returns (bool);
    }
}

/// Gnosis Conditional Tokens `ERC1155` — operator checks for collateral adapters.
const CTF_ERC1155: Address = address!("0x4D97DCd97eC945f40cF65f87097ACe5EA0476045");
#[derive(Deserialize)]
struct NonceResponse {
    nonce: String,
}

#[derive(Deserialize)]
struct DeployedResponse {
    deployed: bool,
}

#[derive(Deserialize)]
struct SubmitResponse {
    #[serde(rename = "transactionID")]
    transaction_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    transaction_hash: String,
    state: String,
}

#[derive(Deserialize)]
struct RelayerTxRecord {
    #[serde(rename = "errorMsg", default)]
    error_msg: Option<String>,
}

async fn relayer_transaction_error_msg(
    http: &Client,
    relayer_base: &str,
    transaction_id: &str,
) -> Option<String> {
    let base = normalize_relayer_base(relayer_base);
    let url = format!("{base}/transaction?id={transaction_id}");
    let resp = http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let txt = resp.text().await.ok()?;
    let rows: Vec<RelayerTxRecord> = serde_json::from_str(&txt).ok()?;
    rows.into_iter().next().and_then(|r| r.error_msg)
}

/// Polymarket CREATE2 Safe for browser-wallet users (`derive_safe_wallet` in `polymarket_client_sdk_v2`).
fn derive_polymarket_safe(eoa: Address) -> Address {
    let mut padded = [0_u8; 32];
    padded[12..].copy_from_slice(eoa.as_slice());
    let salt = keccak256(padded);
    SAFE_FACTORY.create2(salt, SAFE_INIT_CODE_HASH)
}

fn encode_v2_adapter_redeem(condition_id: B256) -> Vec<u8> {
    CtfCollateralAdapter::redeemPositionsCall {
        collateralToken: Address::ZERO,
        parentCollectionId: B256::ZERO,
        conditionId: condition_id,
        indexSets: vec![U256::from(1u64), U256::from(2u64)],
    }
    .abi_encode()
}

/// One inner call for Gnosis `MultiSend.multiSend` (`abi.encodePacked` per sub-tx).
fn gnosis_multisend_pack_inner_call(to: Address, value: U256, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 20 + 32 + 32 + data.len());
    v.push(0u8); // OperationType.Call (delegatecall only on the outer Safe tx)
    v.extend_from_slice(to.as_slice());
    v.extend_from_slice(&value.to_be_bytes::<32>());
    let len = U256::from(data.len());
    v.extend_from_slice(&len.to_be_bytes::<32>());
    v.extend_from_slice(data);
    v
}

fn encode_safe_multisend_calldata_from_packed(packed: Vec<u8>) -> Result<Vec<u8>> {
    if packed.is_empty() {
        bail!("multiSend: empty batch");
    }
    Ok(MultiSend::multiSendCall {
        transactions: packed.into(),
    }
    .abi_encode())
}

/// One Safe tx: single adapter call, or `MultiSend` when `ops.len() > 1`.
fn build_redeem_safe_tx_from_ops(
    mut ops: Vec<(String, Address, Vec<u8>)>,
) -> Result<(Address, Vec<u8>, u8)> {
    if ops.is_empty() {
        bail!("redeem: empty ops");
    }
    if ops.len() == 1 {
        let (_, t, d) = ops.pop().expect("len==1");
        Ok((t, d, 0u8))
    } else {
        let mut packed = Vec::new();
        for (_, t, d) in &ops {
            packed.extend(gnosis_multisend_pack_inner_call(*t, U256::ZERO, d));
        }
        let data = encode_safe_multisend_calldata_from_packed(packed)?;
        Ok((SAFE_MULTISEND, data, 1u8))
    }
}

/// Pack ECDSA signature for Polymarket Safe relayer (see `builder-relayer-client` `splitAndPackSig`).
fn pack_safe_rel_signature(mut sig: [u8; 65]) -> Result<String> {
    let mut v = u16::from(sig[64]);
    match v {
        0 | 1 => v += 31,
        27 | 28 => v += 4,
        _ => bail!("unexpected signature v byte: {}", sig[64]),
    }
    sig[64] = v as u8;
    let r = U256::from_be_slice(&sig[..32]);
    let s = U256::from_be_slice(&sig[32..64]);
    let vb = sig[64] as u64;
    let mut packed = Vec::with_capacity(65);
    packed.extend_from_slice(&r.to_be_bytes::<32>());
    packed.extend_from_slice(&s.to_be_bytes::<32>());
    packed.push(vb as u8);
    Ok(format!(
        "0x{}",
        packed
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}

fn safe_typed_data_digest(
    chain_id: u64,
    safe: Address,
    to: Address,
    data: &[u8],
    operation: u8,
    nonce: &str,
) -> Result<B256> {
    let data_hex = format!(
        "0x{}",
        data.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let json = json!({
        "types": {
            "EIP712Domain": [
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "SafeTx": [
                {"name": "to", "type": "address"},
                {"name": "value", "type": "uint256"},
                {"name": "data", "type": "bytes"},
                {"name": "operation", "type": "uint8"},
                {"name": "safeTxGas", "type": "uint256"},
                {"name": "baseGas", "type": "uint256"},
                {"name": "gasPrice", "type": "uint256"},
                {"name": "gasToken", "type": "address"},
                {"name": "refundReceiver", "type": "address"},
                {"name": "nonce", "type": "uint256"}
            ]
        },
        "primaryType": "SafeTx",
        "domain": {
            "chainId": chain_id,
            "verifyingContract": format!("{safe:#x}")
        },
        "message": {
            "to": format!("{to:#x}"),
            "value": "0",
            "data": data_hex,
            "operation": operation,
            "safeTxGas": "0",
            "baseGas": "0",
            "gasPrice": "0",
            "gasToken": "0x0000000000000000000000000000000000000000",
            "refundReceiver": "0x0000000000000000000000000000000000000000",
            "nonce": nonce
        }
    });
    let td: TypedData = serde_json::from_value(json).context("EIP-712 JSON for SafeTx")?;
    td.eip712_signing_hash()
        .map_err(|e| anyhow::anyhow!("EIP-712 hash: {e}"))
}

async fn relayer_get_nonce(http: &Client, signer: Address) -> Result<String> {
    let url = format!("{RELAYER_HOST}/nonce?address={signer:#x}&type=SAFE");
    let resp = http.get(&url).send().await.context("relayer GET /nonce")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer /nonce failed: {status} — {}", txt.trim());
    }
    let n: NonceResponse =
        serde_json::from_str(&txt).with_context(|| format!("decode /nonce: {}", txt.trim()))?;
    Ok(n.nonce)
}

async fn relayer_deployed(http: &Client, proxy_wallet: Address) -> Result<bool> {
    let url = format!("{RELAYER_HOST}/deployed?address={proxy_wallet:#x}");
    let resp = http
        .get(&url)
        .send()
        .await
        .context("relayer GET /deployed")?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("relayer /deployed failed: {status} — {}", txt.trim());
    }
    let d: DeployedResponse =
        serde_json::from_str(&txt).with_context(|| format!("decode /deployed: {}", txt.trim()))?;
    Ok(d.deployed)
}

/// `eth_getCode` — true if `address` has contract bytecode (EOA / empty returns false).
/// Used for **deposit wallets**: relayer `GET /deployed` is Safe/proxy-oriented and stays false for deposit wallets.
async fn polygon_address_has_contract_code(
    http: &Client,
    rpc_url: &str,
    address: Address,
) -> Result<bool> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1u64,
        "method": "eth_getCode",
        "params": [format!("{address:#x}"), "latest"],
    });
    let resp = http
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("Polygon RPC POST {}", rpc_url.trim_end_matches('/')))?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Polygon RPC HTTP {status} — {}", txt.trim());
    }
    let v: serde_json::Value =
        serde_json::from_str(&txt).with_context(|| format!("decode eth_getCode: {}", txt.trim()))?;
    if let Some(err) = v.get("error") {
        bail!("eth_getCode error: {err}");
    }
    let code = v
        .get("result")
        .and_then(|r| r.as_str())
        .unwrap_or("0x");
    Ok(code.len() > 2 && code != "0x")
}

/// JSON-RPC `eth_call` — returns `result` hex string (no `0x` prefix required in return).
async fn polygon_eth_call(
    http: &Client,
    rpc_url: &str,
    to: Address,
    calldata: &[u8],
) -> Result<String> {
    let data_hex = format!("0x{}", hex::encode(calldata));
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1u64,
        "method": "eth_call",
        "params": [{
            "to": format!("{to:#x}"),
            "data": data_hex,
        }, "latest"]
    });
    let resp = http
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("Polygon eth_call POST {}", rpc_url.trim_end_matches('/')))?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("Polygon RPC HTTP {status} — {}", txt.trim());
    }
    let v: serde_json::Value =
        serde_json::from_str(&txt).with_context(|| format!("decode eth_call: {}", txt.trim()))?;
    if let Some(err) = v.get("error") {
        bail!("eth_call error: {err}");
    }
    v.get("result")
        .and_then(|r| r.as_str())
        .map(String::from)
        .context("eth_call missing result")
}

fn decode_abi_bool_result(result_hex: &str) -> Result<bool> {
    let h = result_hex.strip_prefix("0x").unwrap_or(result_hex);
    if h.len() < 64 {
        bail!("eth_call bool: short result {h}");
    }
    let b = hex::decode(h).context("eth_call result hex")?;
    let w = b
        .get(b.len().saturating_sub(32)..)
        .ok_or_else(|| anyhow::anyhow!("eth_call bool: slice"))?;
    Ok(!w.iter().all(|x| *x == 0))
}

async fn ctf_is_approved_for_all(
    http: &Client,
    rpc_url: &str,
    owner: Address,
    operator: Address,
) -> Result<bool> {
    let call = IERC1155Ops::isApprovedForAllCall {
        account: owner,
        operator,
    };
    let raw = polygon_eth_call(http, rpc_url, CTF_ERC1155, &call.abi_encode()).await?;
    decode_abi_bool_result(&raw)
}

async fn relayer_submit(
    http: &Client,
    relayer_key: &str,
    relayer_key_addr: Address,
    body: serde_json::Value,
) -> Result<SubmitResponse> {
    let resp = http
        .post(format!("{RELAYER_HOST}/submit"))
        .header("RELAYER_API_KEY", relayer_key)
        .header("RELAYER_API_KEY_ADDRESS", format!("{relayer_key_addr:#x}"))
        .json(&body)
        .send()
        .await
        .with_context(|| {
            error!("relayer POST /submit: transport error before HTTP status");
            "relayer POST /submit"
        })?;
    let status = resp.status();
    let txt = resp.text().await.unwrap_or_default();
    let body_trim = txt.trim();
    if !status.is_success() {
        let snippet: String = body_trim.chars().take(512).collect();
        error!(
            status = %status,
            response_snippet = %snippet,
            "relayer POST /submit: HTTP error"
        );
        bail!("relayer /submit failed: {status} — {}", body_trim);
    }
    match serde_json::from_str::<SubmitResponse>(body_trim) {
        Ok(out) => Ok(out),
        Err(e) => {
            let snippet: String = body_trim.chars().take(512).collect();
            error!(
                error = %e,
                response_snippet = %snippet,
                "relayer POST /submit: JSON decode error (HTTP 2xx)"
            );
            Err(e).with_context(|| format!("decode /submit: {body_trim}"))
        }
    }
}

pub(crate) fn parse_condition_id(s: &str) -> Result<B256> {
    let t = s.trim();
    let h = t.strip_prefix("0x").unwrap_or(t);
    let b = hex::decode(h).context("conditionId hex")?;
    if b.len() != 32 {
        bail!("conditionId must be 32 bytes, got {}", b.len());
    }
    Ok(B256::from_slice(&b))
}

pub(crate) fn parse_token_id_u256(s: &str) -> Result<U256> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x") {
        let b = hex::decode(h).context("asset id hex")?;
        return Ok(U256::from_be_slice(&b));
    }
    U256::from_str_radix(t, 10).context("asset id decimal")
}

/// Build adapter calldata for each unique redeemable condition (Data API).
fn collect_redeem_ops(positions: &[DataPosition]) -> Result<Vec<(String, Address, Vec<u8>)>> {
    let mut redeemable: Vec<&DataPosition> = positions
        .iter()
        .filter(|p| {
            p.redeemable
                && p.current_value.is_finite()
                && p.current_value > 0.0
        })
        .collect();
    redeemable.sort_by(|a, b| a.condition_id.cmp(&b.condition_id));
    if redeemable.is_empty() {
        bail!("no redeemable positions from Data API");
    }

    let mut seen = std::collections::HashSet::new();
    let mut ops: Vec<(String, Address, Vec<u8>)> = Vec::new();

    for p in redeemable {
        if !seen.insert(p.condition_id.as_str()) {
            continue;
        }
        let short = p.condition_id.chars().take(10).collect::<String>();
        let condition = match parse_condition_id(&p.condition_id) {
            Ok(c) => c,
            Err(e) => {
                warn!(cond = %p.condition_id, error = %e, "CTF redeem: skip (bad conditionId)");
                continue;
            }
        };
        let adapter = if p.negative_risk {
            NEG_RISK_CTF_COLLATERAL_ADAPTER
        } else {
            CTF_COLLATERAL_ADAPTER
        };
        ops.push((short, adapter, encode_v2_adapter_redeem(condition)));
    }

    if ops.is_empty() {
        bail!("CTF redeem: nothing to redeem (all rows skipped or no redeemable markets)");
    }
    Ok(ops)
}

async fn redeem_via_safe(
    cfg: &Config,
    http: &Client,
    ops: Vec<(String, Address, Vec<u8>)>,
    rel_key: &str,
    rel_addr: Address,
) -> Result<String> {
    let signer: PrivateKeySigner = cfg.private_key.parse().context("parse POLYMARKET_PK")?;
    let derived_safe = derive_polymarket_safe(cfg.signer_address);
    if derived_safe != cfg.funder {
        bail!(
            "POLYMARKET_FUNDER ({:#x}) != derived Safe ({:#x}) for this EOA — check env",
            cfg.funder,
            derived_safe
        );
    }
    if !relayer_deployed(http, cfg.funder).await? {
        bail!("Safe not deployed on-chain yet — use polymarket.com once before redeeming");
    }

    let market_count = ops.len();
    let ids = ops
        .iter()
        .map(|(s, _, _)| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let chunks: Vec<Vec<(String, Address, Vec<u8>)>> = ops
        .chunks(REDEEM_MULTISEND_CHUNK)
        .map(|c| c.to_vec())
        .collect();
    let num_chunks = chunks.len();
    let mut summaries = Vec::new();

    for (i, chunk_ops) in chunks.into_iter().enumerate() {
        let chunk_idx = i + 1;
        let chunk_len = chunk_ops.len();
        let (relay_to, calldata, safe_operation) = build_redeem_safe_tx_from_ops(chunk_ops)?;
        let nonce = relayer_get_nonce(http, cfg.signer_address).await?;
        let digest = safe_typed_data_digest(
            crate::config::POLYGON_CHAIN_ID,
            cfg.funder,
            relay_to,
            &calldata,
            safe_operation,
            &nonce,
        )?;
        let sig = signer
            .sign_message(digest.as_slice())
            .await
            .context("sign SafeTx digest (EIP-191 over EIP-712 hash, relayer-compatible)")?;
        let sig_bytes: [u8; 65] = sig.as_bytes();
        let packed_sig = pack_safe_rel_signature(sig_bytes)?;

        let metadata = format!("polymarket-crypto redeem {}/{}", chunk_idx, num_chunks);
        let req = json!({
            "from": format!("{:#x}", cfg.signer_address),
            "to": format!("{relay_to:#x}"),
            "proxyWallet": format!("{:#x}", cfg.funder),
            "data": format!(
                "0x{}",
                calldata.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ),
            "nonce": nonce,
            "signature": packed_sig,
            "signatureParams": {
                "gasPrice": "0",
                "operation": format!("{safe_operation}"),
                "safeTxnGas": "0",
                "baseGas": "0",
                "gasToken": "0x0000000000000000000000000000000000000000",
                "refundReceiver": "0x0000000000000000000000000000000000000000"
            },
            "type": "SAFE",
            "metadata": metadata,
        });

        let out = relayer_submit(http, rel_key, rel_addr, req).await?;
        let fail_detail = if out.state.eq_ignore_ascii_case("STATE_FAILED") {
            relayer_transaction_error_msg(http, RELAYER_HOST, &out.transaction_id).await
        } else {
            None
        };
        if out.state.eq_ignore_ascii_case("STATE_FAILED") {
            bail!(
                "relayer MultiSend batch {}/{} failed ({} redeems in tx): state={} transactionID={}{}",
                chunk_idx,
                num_chunks,
                chunk_len,
                out.state,
                out.transaction_id,
                fail_detail.map(|d| format!(" — {d}")).unwrap_or_default()
            );
        }

        summaries.push(format!(
            "[{}/{}] {} ({}){}",
            chunk_idx,
            num_chunks,
            out.transaction_id,
            out.state,
            if safe_operation == 1 {
                " MultiSend"
            } else {
                ""
            }
        ));
    }

    Ok(format!(
        "{} market(s) in {} relayer submission(s){} → {} [{}]",
        market_count,
        num_chunks,
        if num_chunks > 1 {
            " (MultiSend batched)"
        } else {
            ""
        },
        summaries.join("; "),
        ids
    ))
}

async fn redeem_via_deposit_wallet(
    cfg: &Config,
    http: &Client,
    ops: Vec<(String, Address, Vec<u8>)>,
    rel_key: &str,
    rel_addr: Address,
) -> Result<String> {
    let relayer_base = relayer_base_url();
    let signer: PrivateKeySigner = cfg.private_key.parse().context("parse POLYMARKET_PK")?;
    let owner = cfg.signer_address;
    let deposit_wallet = cfg.funder;

    let has_code =
        polygon_address_has_contract_code(http, cfg.polygon_rpc_url.as_str(), deposit_wallet)
            .await?;
    if !has_code {
        bail!(
            "Deposit wallet has no contract code on Polygon — deploy with relayer WALLET-CREATE first \
             (verify POLYGON_RPC_URL reaches Polygon mainnet)"
        );
    }

    let rpc_url = cfg.polygon_rpc_url.as_str();
    let mut uniq_adapters: Vec<Address> = ops
        .iter()
        .map(|(_, ad, _)| *ad)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    uniq_adapters.sort_by(|a, b| a.as_slice().cmp(b.as_slice()));

    let mut need_operator_fix: Vec<Address> = Vec::new();
    for ad in &uniq_adapters {
        let ok = ctf_is_approved_for_all(http, rpc_url, deposit_wallet, *ad)
            .await
            .with_context(|| format!("Polygon eth_call isApprovedForAll ({ad:#x})"))?;
        if !ok {
            need_operator_fix.push(*ad);
        }
    }

    if !need_operator_fix.is_empty() {
        submit_collateral_adapter_allowances_for_adapters(cfg, http, &need_operator_fix)
            .await
            .context("relayer WALLET batch (CTF collateral adapter allowances)")?;

        for ad in &uniq_adapters {
            let ok = ctf_is_approved_for_all(http, rpc_url, deposit_wallet, *ad)
                .await
                .with_context(|| format!("re-check isApprovedForAll ({ad:#x})"))?;
            if !ok {
                bail!(
                    "CTF `isApprovedForAll` still false for {ad:#x} after collateral allowance batch — \
                     check Polygon RPC matches mainnet or retry deposit-wallet approvals"
                );
            }
        }
    }

    let market_count = ops.len();
    let ids = ops
        .iter()
        .map(|(s, _, _)| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let chunks: Vec<Vec<(String, Address, Vec<u8>)>> = ops
        .chunks(REDEEM_MULTISEND_CHUNK)
        .map(|c| c.to_vec())
        .collect();
    let num_chunks = chunks.len();
    let mut summaries = Vec::new();

    for (i, chunk_ops) in chunks.into_iter().enumerate() {
        let chunk_idx = i + 1;
        let chunk_len = chunk_ops.len();
        let nonce = relayer_wallet_nonce(http, &relayer_base, owner).await?;
        let deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system time")?
            .as_secs()
            + 240;

        let calls: Vec<(Address, U256, Vec<u8>)> = chunk_ops
            .iter()
            .map(|(_, adapter, data)| (*adapter, U256::ZERO, data.clone()))
            .collect();

        let typed =
            deposit_wallet_batch_typed_data(deposit_wallet, &nonce, deadline, &calls)?;
        let digest = typed
            .eip712_signing_hash()
            .map_err(|e| anyhow::anyhow!("DepositWallet Batch EIP-712 hash: {e}"))?;
        let sig = signer
            .sign_hash(&digest)
            .await
            .context("sign DepositWallet Batch (relayer WALLET)")?;
        let signature = format!("0x{}", hex::encode(sig.as_bytes()));

        let calls_submit: Vec<_> = chunk_ops
            .iter()
            .map(|(_, target, data)| {
                json!({
                    "target": format!("{target:#x}"),
                    "value": "0",
                    "data": hex0x(data),
                })
            })
            .collect();

        let body = json!({
            "type": "WALLET",
            "from": format!("{owner:#x}"),
            "to": format!("{:#x}", DEPOSIT_WALLET_FACTORY_POLYGON),
            "nonce": nonce,
            "signature": signature,
            "depositWalletParams": {
                "depositWallet": format!("{deposit_wallet:#x}"),
                "deadline": deadline.to_string(),
                "calls": calls_submit
            }
        });

        let out =
            submit_relayer_json_retry_on_wallet_busy(http, &relayer_base, rel_key, rel_addr, &body)
                .await?;
        let fail_detail = if out.state.eq_ignore_ascii_case("STATE_FAILED") {
            relayer_transaction_error_msg(http, &relayer_base, &out.transaction_id).await
        } else {
            None
        };
        if out.state.eq_ignore_ascii_case("STATE_FAILED") {
            bail!(
                "relayer deposit-wallet batch {}/{} failed ({} redeems in tx): state={} transactionID={}{}",
                chunk_idx,
                num_chunks,
                chunk_len,
                out.state,
                out.transaction_id,
                fail_detail.map(|d| format!(" — {d}")).unwrap_or_default()
            );
        }

        summaries.push(format!(
            "[{}/{}] {} ({}) deposit-wallet",
            chunk_idx, num_chunks, out.transaction_id, out.state
        ));
    }

    Ok(format!(
        "{} market(s) in {} relayer WALLET submission(s){} → {} [{}]",
        market_count,
        num_chunks,
        if num_chunks > 1 {
            " (batched)"
        } else {
            ""
        },
        summaries.join("; "),
        ids
    ))
}

/// Redeem all redeemable positions from the Data API through the relayer.
///
/// * **`POLYMARKET_SIG_TYPE=2`** — Safe [`execTransaction`] (`SAFE`), optionally batched with **MultiSend**
///   ([`aggregateTransaction`](https://github.com/Polymarket/builder-relayer-client/blob/main/src/builder/safe.ts)).
/// * **`POLYMARKET_SIG_TYPE=3`** — [deposit wallet](https://docs.polymarket.com/trading/deposit-wallet-migration)
///   **`WALLET`** batch (EIP-712 `DepositWallet` / `Batch`).
///
/// Large redemption sets are chunked (`REDEEM_MULTISEND_CHUNK`) so each submission stays within relayer limits.
pub async fn redeem_resolved_positions(
    cfg: &Config,
    http: &Client,
    positions: &[DataPosition],
) -> Result<String> {
    let rel_key = cfg
        .relayer_api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .context(
            "set POLYMARKET_RELAYER_API_KEY (+ POLYMARKET_RELAYER_API_KEY_ADDRESS) — create at \
             polymarket.com → Settings → API (Relayer)",
        )?;
    let rel_addr = cfg
        .relayer_api_key_address
        .context("POLYMARKET_RELAYER_API_KEY_ADDRESS")?;

    let ops = collect_redeem_ops(positions)?;

    match cfg.sig_type {
        SignatureType::PolyGnosisSafe => redeem_via_safe(cfg, http, ops, rel_key, rel_addr).await,
        SignatureType::Poly1271 => {
            redeem_via_deposit_wallet(cfg, http, ops, rel_key, rel_addr).await
        }
        _ => {
            bail!(
                "CTF redeem via relayer supports POLYMARKET_SIG_TYPE=2 (Gnosis Safe) or 3 (deposit wallet / POLY_1271). \
                 For EOA/proxy wallets use polymarket.com Portfolio or the official CLI."
            );
        }
    }
}
