//! CLOB V2 [`POLY_1271`](https://docs.polymarket.com/trading/deposit-wallet-migration) (signature type 3)
//! order signatures — ports `@polymarket/clob-client-v2` `ExchangeOrderBuilderV2.buildOrderSignature`
//! (see workspace `.tmp-clob-sign-snippet.js` / unpkg bundle).

use std::string::String;

use alloy_primitives::{keccak256, Address, PrimitiveSignature, B256, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{eip712_domain, sol, Eip712Domain, SolStruct};
use anyhow::Result;

// Nested `Order` must keep the canonical Solidity name `Order` for EIP-712.
sol! {
    #[derive(Debug)]
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint8 side;
        uint8 signatureType;
        uint256 timestamp;
        bytes32 metadata;
        bytes32 builder;
    }

    #[derive(Debug)]
    struct TypedDataSign {
        Order contents;
        string name;
        string version;
        uint256 chainId;
        address verifyingContract;
        bytes32 salt;
    }
}

const ORDER_TYPE_STRING: &str = "Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";

fn order_type_hash() -> B256 {
    keccak256(ORDER_TYPE_STRING.as_bytes())
}

fn domain_type_hash() -> B256 {
    keccak256(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")
}

fn ctf_exchange_name_hash() -> B256 {
    keccak256(b"Polymarket CTF Exchange")
}

fn ctf_exchange_version_hash() -> B256 {
    keccak256(b"2")
}

fn pad_u256_word(v: U256, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&v.to_be_bytes::<32>());
}

fn pad_address_word(a: Address, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&[0u8; 12]);
    buf.extend_from_slice(a.as_slice());
}

/// `ExchangeOrderBuilderV2` `appDomainSep` (constructor) — keccak of ABI-encoded domain tuple.
pub fn app_domain_separator(exchange: Address, chain_id: u64) -> B256 {
    let mut enc = Vec::with_capacity(5 * 32);
    enc.extend_from_slice(domain_type_hash().as_slice());
    enc.extend_from_slice(ctf_exchange_name_hash().as_slice());
    enc.extend_from_slice(ctf_exchange_version_hash().as_slice());
    pad_u256_word(U256::from(chain_id), &mut enc);
    pad_address_word(exchange, &mut enc);
    keccak256(&enc)
}

/// Inner `contents` digest — matches `encodeAbiParameters` in `buildOrderSignature`.
fn order_contents_hash(
    salt: U256,
    maker: Address,
    signer: Address,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    side: u8,
    signature_type: u8,
    timestamp: U256,
    metadata: B256,
    builder: B256,
) -> B256 {
    let mut enc = Vec::with_capacity(12 * 32);
    enc.extend_from_slice(order_type_hash().as_slice());
    pad_u256_word(salt, &mut enc);
    pad_address_word(maker, &mut enc);
    pad_address_word(signer, &mut enc);
    pad_u256_word(token_id, &mut enc);
    pad_u256_word(maker_amount, &mut enc);
    pad_u256_word(taker_amount, &mut enc);
    pad_u256_word(U256::from(side), &mut enc);
    pad_u256_word(U256::from(signature_type), &mut enc);
    pad_u256_word(timestamp, &mut enc);
    enc.extend_from_slice(metadata.as_slice());
    enc.extend_from_slice(builder.as_slice());
    keccak256(&enc)
}

fn exchange_domain(exchange: Address, chain_id: u64) -> Eip712Domain {
    eip712_domain! {
        name: "Polymarket CTF Exchange",
        version: "2",
        chain_id: chain_id,
        verifying_contract: exchange,
    }
}

/// Full Polymarket `POLY_1271` wire signature for a CLOB V2 order (`0x` + hex).
pub async fn sign_poly1271_v2_order(
    signer: &PrivateKeySigner,
    exchange: Address,
    chain_id: u64,
    deposit_wallet: Address,
    salt: U256,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    side: u8,
    timestamp_ms: U256,
    metadata: B256,
    builder: B256,
) -> Result<String> {
    let contents = Order {
        salt,
        maker: deposit_wallet,
        signer: deposit_wallet,
        tokenId: token_id,
        makerAmount: maker_amount,
        takerAmount: taker_amount,
        side,
        signatureType: 3,
        timestamp: timestamp_ms,
        metadata,
        builder,
    };

    let typed = TypedDataSign {
        contents,
        name: String::from("DepositWallet"),
        version: String::from("1"),
        chainId: U256::from(chain_id),
        verifyingContract: deposit_wallet,
        salt: B256::ZERO,
    };

    let domain = exchange_domain(exchange, chain_id);
    let signing_hash = typed.eip712_signing_hash(&domain);
    let sig = signer.sign_hash(&signing_hash).await?;

    let contents_h = order_contents_hash(
        salt,
        deposit_wallet,
        deposit_wallet,
        token_id,
        maker_amount,
        taker_amount,
        side,
        3,
        timestamp_ms,
        metadata,
        builder,
    );
    let app_sep = app_domain_separator(exchange, chain_id);

    assemble_wire_signature(&sig, app_sep, contents_h)
}

fn assemble_wire_signature(
    inner_sig: &PrimitiveSignature,
    app_domain_sep: B256,
    contents_hash: B256,
) -> Result<String> {
    let mut hex_body = String::with_capacity(260);
    hex_body.push_str(&hex::encode(inner_sig.as_bytes()));
    hex_body.push_str(&hex::encode(app_domain_sep.as_slice()));
    hex_body.push_str(&hex::encode(contents_hash.as_slice()));
    hex_body.push_str(&hex::encode(ORDER_TYPE_STRING.as_bytes()));
    hex_body.push_str(&format!("{:04x}", ORDER_TYPE_STRING.len()));
    Ok(format!("0x{hex_body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::POLYGON_CHAIN_ID;
    use alloy_primitives::address;

    #[test]
    fn order_type_hash_stable() {
        let h = order_type_hash();
        assert_ne!(h, B256::ZERO);
        assert_eq!(h, keccak256(ORDER_TYPE_STRING.as_bytes()));
    }

    #[test]
    fn app_domain_matches_manual_keccak() {
        let ex = address!("0xE111180000d2663C0091e4f400237545B87B996B");
        let d = app_domain_separator(ex, POLYGON_CHAIN_ID);
        let mut enc = Vec::with_capacity(160);
        enc.extend_from_slice(domain_type_hash().as_slice());
        enc.extend_from_slice(ctf_exchange_name_hash().as_slice());
        enc.extend_from_slice(ctf_exchange_version_hash().as_slice());
        pad_u256_word(U256::from(POLYGON_CHAIN_ID), &mut enc);
        pad_address_word(ex, &mut enc);
        assert_eq!(d, keccak256(&enc));
    }

    #[tokio::test]
    async fn poly1271_signature_non_empty() {
        let pk = std::env::var("POLY1271_TEST_PK").unwrap_or_else(|_| {
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
        });
        let signer: PrivateKeySigner = pk.parse().expect("test pk");
        let ex = address!("0xE111180000d2663C0091e4f400237545B87B996B");
        let dw = address!("0xfaea0f08159fcf2f573fe24e9e989b0d48f7651b");
        let sig = sign_poly1271_v2_order(
            &signer,
            ex,
            POLYGON_CHAIN_ID,
            dw,
            U256::from(1u64),
            U256::from(2u64),
            U256::from(3u64),
            U256::from(4u64),
            0,
            U256::from(1_700_000_000_000u64),
            B256::ZERO,
            B256::ZERO,
        )
        .await
        .expect("sign");
        assert!(sig.starts_with("0x"));
        assert!(sig.len() > 130);
    }
}
