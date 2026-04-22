//! Polymarket Bridge API — Solana (`svm`) deposit address for USDC bridging.

use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

const BRIDGE_DEPOSIT_URL: &str = "https://bridge.polymarket.com/deposit";
const BRIDGE_SUPPORTED_ASSETS_URL: &str = "https://bridge.polymarket.com/supported-assets";

/// Circle USDC on Solana mainnet (SPL mint). Use in Solana Pay `spl-token` and deposit UI copy.
pub const SOLANA_MAINNET_USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Solana Pay transfer-request URL using only `recipient` (pathname) and `spl-token` (query).
/// Omitting `amount` is valid: the wallet should prompt for the amount ([spec](https://docs.solanapay.com/spec)).
pub fn solana_pay_transfer_url(recipient: &str, spl_token_mint: &str) -> String {
    let recipient = recipient.trim();
    let mint = spl_token_mint.trim();
    format!("solana:{recipient}?spl-token={mint}")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SupportedAssetsResponse {
    supported_assets: Vec<SupportedAssetRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SupportedAssetRow {
    chain_name: String,
    token: SupportedTokenMeta,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SupportedTokenMeta {
    symbol: String,
}

/// Fails if the bridge no longer lists Solana + USDC (see Polymarket `/supported-assets`).
pub async fn ensure_solana_usdc_supported(client: &reqwest::Client) -> Result<()> {
    let resp = client
        .get(BRIDGE_SUPPORTED_ASSETS_URL)
        .send()
        .await
        .context("bridge.polymarket.com supported-assets request")?;
    let status = resp.status();
    let text = resp.text().await.context("supported-assets read body")?;
    if !status.is_success() {
        anyhow::bail!("supported-assets HTTP {status}: {text}");
    }
    let parsed: SupportedAssetsResponse =
        serde_json::from_str(&text).context("supported-assets JSON")?;
    let ok = parsed.supported_assets.iter().any(|row| {
        row.chain_name.eq_ignore_ascii_case("solana")
            && row.token.symbol.eq_ignore_ascii_case("USDC")
    });
    if !ok {
        anyhow::bail!(
            "bridge supported-assets: no Solana USDC entry — chain or token may be unavailable"
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct DepositResponse {
    address: DepositAddresses,
}

#[derive(Debug, Deserialize)]
struct DepositAddresses {
    svm: String,
}

pub async fn fetch_svm_deposit_address(
    client: &reqwest::Client,
    polymarket_wallet: Address,
) -> Result<String> {
    ensure_solana_usdc_supported(client).await?;

    let addr = format!("{:#x}", polymarket_wallet);
    let body = serde_json::json!({ "address": addr });
    let resp = client
        .post(BRIDGE_DEPOSIT_URL)
        .json(&body)
        .send()
        .await
        .context("bridge.polymarket.com deposit request")?;
    let status = resp.status();
    let text = resp.text().await.context("bridge deposit read body")?;
    if !status.is_success() {
        anyhow::bail!("bridge deposit HTTP {status}: {text}");
    }
    let parsed: DepositResponse =
        serde_json::from_str(&text).context("bridge deposit JSON")?;
    let svm = parsed.address.svm.trim().to_string();
    if svm.is_empty() {
        anyhow::bail!("bridge deposit: empty svm address");
    }
    Ok(svm)
}

/// Dense Unicode QR (two terminal rows per module) for terminal display.
/// Works for raw addresses or full Solana Pay `solana:…` URLs.
pub fn svm_address_qr_unicode(svm_address: &str) -> Result<String> {
    use qrcode::{render::unicode, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(svm_address.as_bytes(), EcLevel::M)
        .context("QR encode")?;
    Ok(code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .quiet_zone(true)
        .build())
}

#[cfg(test)]
#[test]
fn supported_assets_json_finds_solana_usdc() {
    let j = r#"{"supportedAssets":[{"chainId":"x","chainName":"Solana","token":{"name":"USD Coin","symbol":"USDC","address":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","decimals":6},"minCheckoutUsd":2},{"chainName":"Ethereum","token":{"symbol":"USDC"}}]}"#;
    let parsed: SupportedAssetsResponse = serde_json::from_str(j).unwrap();
    let ok = parsed.supported_assets.iter().any(|row| {
        row.chain_name.eq_ignore_ascii_case("solana")
            && row.token.symbol.eq_ignore_ascii_case("USDC")
    });
    assert!(ok);
}

#[cfg(test)]
#[test]
fn solana_pay_url_recipient_and_mint_only() {
    let url = solana_pay_transfer_url(
        "Gje4vt9gwSzbq1U9vbwgRWuyVfGqYoPrX8f9VZWo6cuQ",
        SOLANA_MAINNET_USDC_MINT,
    );
    assert_eq!(
        url,
        "solana:Gje4vt9gwSzbq1U9vbwgRWuyVfGqYoPrX8f9VZWo6cuQ?spl-token=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
    );
}
