//! Deterministic [deposit wallet](https://docs.polymarket.com/trading/deposit-wallet-migration) address
//! (CREATE2 + Solady ERC-1967 init code hash). Mirrors `@polymarket/builder-relayer-client`
//! `deriveDepositWallet` ([`derive.ts`](https://github.com/Polymarket/builder-relayer-client/blob/main/src/builder/derive.ts)).

use alloy_primitives::{address, keccak256, Address, B256, U256};

/// Polygon mainnet — from Polymarket `getContractConfig(137)`.
pub const DEPOSIT_WALLET_FACTORY_POLYGON: Address =
    address!("0x00000000000Fb5C9ADea0298D729A0CB3823Cc07");

/// Polygon mainnet deposit wallet implementation (ERC-1967).
pub const DEPOSIT_WALLET_IMPLEMENTATION_POLYGON: Address =
    address!("0x58CA52ebe0DadfdF531Cde7062e76746de4Db1eB");

const ERC1967_CONST1: [u8; 32] = [
    0xcc, 0x37, 0x35, 0xa9, 0x20, 0xa3, 0xca, 0x50, 0x5d, 0x38, 0x2b, 0xbc, 0x54, 0x5a, 0xf4, 0x3d,
    0x60, 0x00, 0x80, 0x3e, 0x60, 0x38, 0x57, 0x3d, 0x60, 0x00, 0xfd, 0x5b, 0x3d, 0x60, 0x00, 0xf3,
];
const ERC1967_CONST2: [u8; 32] = [
    0x51, 0x55, 0xf3, 0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x36, 0x3d, 0x7f, 0x36, 0x08, 0x94, 0xa1,
    0x3b, 0xa1, 0xa3, 0x21, 0x06, 0x67, 0xc8, 0x28, 0x49, 0x2d, 0xb9, 0x8d, 0xca, 0x3e, 0x20, 0x76,
];

fn erc1967_prefix_u256() -> U256 {
    U256::from_be_slice(&hex::decode("61003d3d8160233d3973").expect("static hex"))
}

fn init_code_hash_erc1967(implementation: Address, args: &[u8]) -> B256 {
    let n = args.len();
    let combined: U256 = erc1967_prefix_u256() + (U256::from(n) << 56);
    let mut prefix10 = [0u8; 10];
    prefix10.copy_from_slice(&combined.to_be_bytes::<32>()[22..]);

    let mut buf =
        Vec::with_capacity(10 + 20 + 2 + ERC1967_CONST2.len() + ERC1967_CONST1.len() + args.len());
    buf.extend_from_slice(&prefix10);
    buf.extend_from_slice(implementation.as_slice());
    buf.extend_from_slice(&[0x60, 0x09]);
    buf.extend_from_slice(&ERC1967_CONST2);
    buf.extend_from_slice(&ERC1967_CONST1);
    buf.extend_from_slice(args);
    keccak256(&buf)
}

fn wallet_id_bytes32(owner: Address) -> B256 {
    let mut b = [0u8; 32];
    b[12..].copy_from_slice(owner.as_slice());
    B256::from(b)
}

/// `abi.encode(factory, walletId)` where `walletId = bytes32(owner)`.
fn factory_wallet_args(factory: Address, owner: Address) -> Vec<u8> {
    let mut out = vec![0u8; 64];
    out[12..32].copy_from_slice(factory.as_slice());
    out[32..64].copy_from_slice(wallet_id_bytes32(owner).as_slice());
    out
}

/// Predicted deposit wallet for `owner` on Polygon mainnet (`chain_id` 137).
pub fn derive_deposit_wallet_address_polygon(owner: Address) -> Address {
    derive_deposit_wallet_address(
        owner,
        DEPOSIT_WALLET_FACTORY_POLYGON,
        DEPOSIT_WALLET_IMPLEMENTATION_POLYGON,
    )
}

/// Low-level: match relayer client `deriveDepositWallet(owner, factory, implementation)`.
pub fn derive_deposit_wallet_address(
    owner: Address,
    factory: Address,
    implementation: Address,
) -> Address {
    let args = factory_wallet_args(factory, owner);
    let salt = keccak256(&args);
    let bytecode_hash = init_code_hash_erc1967(implementation, &args);
    create2_address(factory, salt.into(), bytecode_hash)
}

/// Standard CREATE2 address (`keccak256(0xff ++ from ++ salt ++ init_code_hash)`).
fn create2_address(from: Address, salt: B256, init_code_hash: B256) -> Address {
    let mut data = Vec::with_capacity(1 + 20 + 32 + 32);
    data.push(0xff);
    data.extend_from_slice(from.as_slice());
    data.extend_from_slice(salt.as_slice());
    data.extend_from_slice(init_code_hash.as_slice());
    let h = keccak256(&data);
    Address::from_slice(&h[12..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Golden address from `@polymarket/builder-relayer-client` `deriveDepositWallet` (Polygon).
    #[test]
    fn derive_matches_ts_reference_vector() {
        let owner = Address::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let got = derive_deposit_wallet_address_polygon(owner);
        let expected = Address::from_str("0xfaea0f08159fcf2f573fe24e9e989b0d48f7651b").unwrap();
        assert_eq!(got, expected);
    }
}
