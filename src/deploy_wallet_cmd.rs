//! CLI: `polymarket-crypto deploy-wallet` — `WALLET-CREATE` via Polymarket relayer (same auth as redeem).

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, Result};
use std::str::FromStr;

use crate::deposit_wallet::{
    derive_deposit_wallet_address_polygon, DEPOSIT_WALLET_FACTORY_POLYGON,
};
use crate::polymarket_relayer::{submit_wallet_create, wait_relayer_transaction};

/// Load env for deploy only (does not require `POLYMARKET_FUNDER` / full trading config).
pub fn load_deploy_env() -> Result<DeployWalletEnv> {
    let _ = dotenvy::dotenv();
    let poly_pk =
        std::env::var("POLYMARKET_PK").context("POLYMARKET_PK (owner EOA private key)")?;

    let relayer_url = std::env::var("RELAYER_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://relayer-v2.polymarket.com".into());

    let relayer_api_key = std::env::var("POLYMARKET_RELAYER_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .context(
            "POLYMARKET_RELAYER_API_KEY — create at polymarket.com → Settings → API (Relayer)",
        )?;
    let relayer_api_key_address = std::env::var("POLYMARKET_RELAYER_API_KEY_ADDRESS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .context("POLYMARKET_RELAYER_API_KEY_ADDRESS (paired with the relayer API key)")?;
    let relayer_api_key_address = Address::from_str(relayer_api_key_address.trim())
        .context("POLYMARKET_RELAYER_API_KEY_ADDRESS")?;

    Ok(DeployWalletEnv {
        poly_pk,
        relayer_url,
        relayer_api_key,
        relayer_api_key_address,
    })
}

pub struct DeployWalletEnv {
    pub poly_pk: String,
    pub relayer_url: String,
    pub relayer_api_key: String,
    pub relayer_api_key_address: Address,
}

pub async fn run() -> Result<()> {
    let env = load_deploy_env()?;
    let signer: PrivateKeySigner = env.poly_pk.parse().context("parse POLYMARKET_PK")?;
    let owner = signer.address();
    let predicted = derive_deposit_wallet_address_polygon(owner);

    println!("\n━━━ Deposit wallet (WALLET-CREATE) ━━━\n");
    println!("owner (EOA)         : {owner:#x}");
    println!(
        "factory             : {:#x}",
        DEPOSIT_WALLET_FACTORY_POLYGON
    );
    println!("predicted (CREATE2) : {predicted:#x}");
    println!("relayer             : {}", env.relayer_url.trim());
    println!();

    let http = crate::net::reqwest_client()?;
    let submit = submit_wallet_create(
        &http,
        &env.relayer_url,
        owner,
        DEPOSIT_WALLET_FACTORY_POLYGON,
        &env.relayer_api_key,
        env.relayer_api_key_address,
    )
    .await
    .context("POST /submit WALLET-CREATE")?;

    println!("submitted: transactionID={}", submit.transaction_id);
    println!("          initial state: {}", submit.state);
    if let Some(h) = &submit.transaction_hash {
        println!("          tx hash: {h}");
    }
    println!("\nWaiting for relayer (STATE_MINED / STATE_CONFIRMED)…\n");

    let final_row =
        wait_relayer_transaction(&http, &env.relayer_url, &submit.transaction_id, 100, 2_000)
            .await
            .context("wait relayer")?;

    println!("final state: {}", final_row.state);
    if let Some(h) = &final_row.transaction_hash {
        println!("on-chain: {h}");
    }
    println!("Set in `.env` for trading with deposit wallet + POLY_1271:");
    println!("  POLYMARKET_FUNDER={predicted:#x}");
    println!("  POLYMARKET_SIG_TYPE=3");
    println!();
    Ok(())
}
