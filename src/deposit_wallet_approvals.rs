//! Relayer [`WALLET`](https://docs.polymarket.com/trading/deposit-wallet-migration#submit-a-deposit-wallet-batch)
//! batch: EIP-712 `DepositWallet` `Batch` + `POST /submit`.
//!
//! Submits standard **pUSD** `approve` + **CTF** `setApprovalForAll` for CTF Exchange V2,
//! Neg-risk CTF Exchange V2, and **CtfCollateralAdapter** / **NegRiskCtfCollateralAdapter**
//! (required for on-chain redeem/split/merge via adapters — [Contracts](https://docs.polymarket.com/resources/contracts)).

use alloy_dyn_abi::eip712::TypedData;
use alloy_primitives::{address, Address, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolCall};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use serde_json::json;

use crate::config::{Config, SignatureType, POLYGON_CHAIN_ID};
use crate::deposit_wallet::DEPOSIT_WALLET_FACTORY_POLYGON;
use crate::polymarket_relayer::{
    relayer_wallet_nonce, submit_relayer_json, wait_relayer_transaction,
};

/// pUSD (`CollateralToken` proxy).
const PUSD: Address = address!("0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB");
/// Conditional Tokens Framework.
const CTF: Address = address!("0x4D97DCd97eC945f40cF65f87097ACe5EA0476045");
const CTF_EXCHANGE_V2: Address = address!("0xE111180000d2663C0091e4f400237545B87B996B");
const NEG_RISK_CTF_EXCHANGE_V2: Address = address!("0xe2222d279d744050d28e00520010520000310F59");
/// Same deployments as [`crate::redeem`] — relayer redeem routes through these adapters.
const CTF_COLLATERAL_ADAPTER: Address =
    address!("0xAdA100Db00Ca00073811820692005400218FcE1f");
const NEG_RISK_CTF_COLLATERAL_ADAPTER: Address =
    address!("0xadA2005600Dec949baf300f4C6120000bDB6eAab");

sol! {
    interface IERC20Approve {
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

sol! {
    interface IERC1155Approval {
        function setApprovalForAll(address operator, bool approved) external;
    }
}

pub(crate) fn relayer_base_url() -> String {
    std::env::var("RELAYER_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://relayer-v2.polymarket.com".into())
}

pub(crate) fn hex0x(data: &[u8]) -> String {
    format!("0x{}", hex::encode(data))
}

fn approval_calls() -> Vec<(Address, U256, Vec<u8>)> {
    let max = U256::MAX;
    vec![
        (
            PUSD,
            U256::ZERO,
            IERC20Approve::approveCall {
                spender: CTF_EXCHANGE_V2,
                amount: max,
            }
            .abi_encode(),
        ),
        (
            PUSD,
            U256::ZERO,
            IERC20Approve::approveCall {
                spender: NEG_RISK_CTF_EXCHANGE_V2,
                amount: max,
            }
            .abi_encode(),
        ),
        (
            PUSD,
            U256::ZERO,
            IERC20Approve::approveCall {
                spender: CTF_COLLATERAL_ADAPTER,
                amount: max,
            }
            .abi_encode(),
        ),
        (
            PUSD,
            U256::ZERO,
            IERC20Approve::approveCall {
                spender: NEG_RISK_CTF_COLLATERAL_ADAPTER,
                amount: max,
            }
            .abi_encode(),
        ),
        (
            CTF,
            U256::ZERO,
            IERC1155Approval::setApprovalForAllCall {
                operator: CTF_EXCHANGE_V2,
                approved: true,
            }
            .abi_encode(),
        ),
        (
            CTF,
            U256::ZERO,
            IERC1155Approval::setApprovalForAllCall {
                operator: NEG_RISK_CTF_EXCHANGE_V2,
                approved: true,
            }
            .abi_encode(),
        ),
        (
            CTF,
            U256::ZERO,
            IERC1155Approval::setApprovalForAllCall {
                operator: CTF_COLLATERAL_ADAPTER,
                approved: true,
            }
            .abi_encode(),
        ),
        (
            CTF,
            U256::ZERO,
            IERC1155Approval::setApprovalForAllCall {
                operator: NEG_RISK_CTF_COLLATERAL_ADAPTER,
                approved: true,
            }
            .abi_encode(),
        ),
    ]
}

/// pUSD + CTF `setApprovalForAll` per collateral adapter (redeem / merge / split via adapters).
fn calls_for_collateral_adapters(adapters: &[Address]) -> Vec<(Address, U256, Vec<u8>)> {
    let max = U256::MAX;
    let mut out = Vec::with_capacity(adapters.len().saturating_mul(2));
    for ad in adapters {
        out.push((
            PUSD,
            U256::ZERO,
            IERC20Approve::approveCall {
                spender: *ad,
                amount: max,
            }
            .abi_encode(),
        ));
        out.push((
            CTF,
            U256::ZERO,
            IERC1155Approval::setApprovalForAllCall {
                operator: *ad,
                approved: true,
            }
            .abi_encode(),
        ));
    }
    out
}

/// Submit arbitrary WALLET `calls` (EIP-712 batch + relayer). Shared by full approvals and targeted adapter fixups.
pub(crate) async fn submit_deposit_wallet_calls(
    cfg: &Config,
    http: &Client,
    calls: Vec<(Address, U256, Vec<u8>)>,
) -> Result<String> {
    if cfg.sig_type != SignatureType::Poly1271 {
        bail!(
            "deposit-wallet relayer batches require POLYMARKET_SIG_TYPE=3 (POLY_1271)"
        );
    }
    if calls.is_empty() {
        bail!("deposit-wallet batch: empty calls");
    }
    let rel_key = cfg
        .relayer_api_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .context(
            "set POLYMARKET_RELAYER_API_KEY (+ POLYMARKET_RELAYER_API_KEY_ADDRESS) — same as CTF redeem",
        )?;
    let rel_addr = cfg
        .relayer_api_key_address
        .context("POLYMARKET_RELAYER_API_KEY_ADDRESS")?;

    let signer: PrivateKeySigner = cfg.private_key.parse().context("parse POLYMARKET_PK")?;
    let owner = signer.address();
    let deposit_wallet = cfg.funder;

    let relayer_url = relayer_base_url();
    let nonce = relayer_wallet_nonce(http, &relayer_url, owner).await?;

    let deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system time")?
        .as_secs()
        + 240;

    let typed = deposit_wallet_batch_typed_data(deposit_wallet, &nonce, deadline, &calls)?;
    let digest = typed
        .eip712_signing_hash()
        .map_err(|e| anyhow!("EIP-712 signing hash: {e}"))?;
    let sig = signer.sign_hash(&digest).await?;
    let signature = format!("0x{}", hex::encode(sig.as_bytes()));

    let calls_submit: Vec<_> = calls
        .iter()
        .map(|(target, value, data)| {
            json!({
                "target": format!("{target:#x}"),
                "value": value.to_string(),
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

    let submitted = submit_relayer_json(http, &relayer_url, rel_key, rel_addr, &body).await?;
    let txid = submitted.transaction_id.clone();
    let final_row = wait_relayer_transaction(http, &relayer_url, &txid, 120, 2_000).await?;
    let mut out = format!(
        "deposit-wallet batch mined (state={})",
        final_row.state
    );
    if let Some(h) = &final_row.transaction_hash {
        out.push_str(&format!(" tx={h}"));
    }
    Ok(out)
}

/// When redeem detects missing CTF operator approvals for collateral adapters, submit only those fixes (smaller batch).
pub(crate) async fn submit_collateral_adapter_allowances_for_adapters(
    cfg: &Config,
    http: &Client,
    adapters: &[Address],
) -> Result<String> {
    if adapters.is_empty() {
        bail!("collateral adapter allowance batch: empty adapters");
    }
    let calls = calls_for_collateral_adapters(adapters);
    submit_deposit_wallet_calls(cfg, http, calls).await
}

/// Build EIP-712 signing hash for `DepositWallet` / `Batch` (matches `@polymarket/builder-relayer-client`).
pub(crate) fn deposit_wallet_batch_typed_data(
    deposit_wallet: Address,
    nonce: &str,
    deadline: u64,
    calls: &[(Address, U256, Vec<u8>)],
) -> Result<TypedData> {
    let calls_json: Vec<_> = calls
        .iter()
        .map(|(target, value, data)| {
            json!({
                "target": format!("{target:#x}"),
                "value": value.to_string(),
                "data": hex0x(data),
            })
        })
        .collect();

    let v = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "Call": [
                {"name": "target", "type": "address"},
                {"name": "value", "type": "uint256"},
                {"name": "data", "type": "bytes"}
            ],
            "Batch": [
                {"name": "wallet", "type": "address"},
                {"name": "nonce", "type": "uint256"},
                {"name": "deadline", "type": "uint256"},
                {"name": "calls", "type": "Call[]"}
            ]
        },
        "primaryType": "Batch",
        "domain": {
            "name": "DepositWallet",
            "version": "1",
            "chainId": POLYGON_CHAIN_ID,
            "verifyingContract": format!("{deposit_wallet:#x}")
        },
        "message": {
            "wallet": format!("{deposit_wallet:#x}"),
            "nonce": nonce,
            "deadline": deadline.to_string(),
            "calls": calls_json
        }
    });
    serde_json::from_value(v).context("EIP-712 JSON (DepositWallet Batch)")
}

/// Submit approval batch via relayer; returns a short status line for the TUI.
pub async fn submit_deposit_wallet_approvals(cfg: &Config, http: &Client) -> Result<String> {
    if cfg.sig_type != SignatureType::Poly1271 {
        bail!(
            "deposit-wallet approvals (relayer WALLET) require POLYMARKET_SIG_TYPE=3 (POLY_1271); \
             Safe/EOA wallets use other approval flows"
        );
    }
    submit_deposit_wallet_calls(cfg, http, approval_calls()).await
}
