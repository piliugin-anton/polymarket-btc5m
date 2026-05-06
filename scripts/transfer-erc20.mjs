#!/usr/bin/env node
/**
 * ERC-20 transfer from the wallet derived from POLYMARKET_PK.
 *
 * Default: transfer from the EOA address (signer of POLYMARKET_PK).
 * Optional --from-deposit: transfer from Polymarket deposit wallet (proxy); uses relayer WALLET batch
 * (same pattern as src/deposit_wallet_approvals.rs).
 *
 * Required env:
 *   POLYMARKET_PK       — 0x-prefixed hex private key (owner EOA)
 *   POLYGON_RPC_URL     — Polygon HTTP RPC
 *
 * With --from-deposit:
 *   POLYMARKET_RELAYER_API_KEY
 *   POLYMARKET_RELAYER_API_KEY_ADDRESS
 * Optional: RELAYER_URL (default https://relayer-v2.polymarket.com)
 *
 * Usage:
 *   node scripts/transfer-erc20.mjs --token <ERC20> --to <address> [--amount <human>] [--decimals 6]
 *   node scripts/transfer-erc20.mjs --from-deposit --token <ERC20> --to <address> --amount <human>
 *
 * If --amount is omitted, the full token balance of the sender is transferred (EOA or deposit wallet).
 */

import "dotenv/config";
import {
  concat,
  encodeAbiParameters,
  encodeFunctionData,
  getAddress,
  getCreate2Address,
  http,
  isAddress,
  keccak256,
  pad,
  parseUnits,
  toHex,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { polygon } from "viem/chains";
import { createPublicClient, createWalletClient } from "viem";

const POLYGON_CHAIN_ID = 137;

const DEPOSIT_WALLET_FACTORY_POLYGON = "0x00000000000Fb5C9ADea0298D729A0CB3823Cc07";
const DEPOSIT_WALLET_IMPLEMENTATION_POLYGON =
  "0x58CA52ebe0DadfdF531Cde7062e76746de4Db1eB";

/** Golden vector from src/deposit_wallet.rs / builder-relayer-client deriveDepositWallet */
const DERIVE_TEST_OWNER = "0x1111111111111111111111111111111111111111";
const DERIVE_TEST_EXPECTED = "0xfaea0f08159fcf2f573fe24e9e989b0d48f7651b";

const ERC1967_CONST1 =
  "0xcc3735a920a3ca505d382bbc545af43d6000803e6038573d6000fd5b3d6000f3";
const ERC1967_CONST2 =
  "0x5155f3363d3d373d3d363d7f360894a13ba1a3210667c828492db98dca3e2076";
const ERC1967_PREFIX = 0x61003d3d8160233d3973n;

function initCodeHashERC1967(implementation, args) {
  const n = BigInt((args.length - 2) / 2);
  const combined = ERC1967_PREFIX + (n << 56n);
  return keccak256(
    concat([
      toHex(combined, { size: 10 }),
      implementation,
      "0x6009",
      ERC1967_CONST2,
      ERC1967_CONST1,
      args,
    ]),
  );
}

function deriveDepositWallet(owner, factory, implementation) {
  const walletId = pad(owner, { dir: "left", size: 32 });
  const args = encodeAbiParameters(
    [{ type: "address" }, { type: "bytes32" }],
    [getAddress(factory), walletId],
  );
  const salt = keccak256(args);
  const bytecodeHash = initCodeHashERC1967(getAddress(implementation), args);
  return getCreate2Address({
    from: getAddress(factory),
    salt,
    bytecodeHash,
  });
}

const erc20Abi = [
  {
    type: "function",
    name: "balanceOf",
    stateMutability: "view",
    inputs: [{ name: "account", type: "address" }],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "decimals",
    stateMutability: "view",
    inputs: [],
    outputs: [{ type: "uint8" }],
  },
  {
    type: "function",
    name: "transfer",
    stateMutability: "nonpayable",
    inputs: [
      { name: "to", type: "address" },
      { name: "amount", type: "uint256" },
    ],
    outputs: [{ type: "bool" }],
  },
];

function normalizePrivateKey(pk) {
  const s = String(pk ?? "").trim();
  if (!s) throw new Error("POLYMARKET_PK is missing");
  const hex = s.startsWith("0x") ? s : `0x${s}`;
  if (!/^0x[0-9a-fA-F]{64}$/.test(hex)) {
    throw new Error("POLYMARKET_PK must be 32-byte hex (with optional 0x prefix)");
  }
  return hex;
}

function parseArgs(argv) {
  const out = {
    fromDeposit: false,
    token: null,
    to: null,
    amountStr: null,
    weiStr: null,
    decimals: null,
    verifyDeriveOnly: false,
  };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--from-deposit") out.fromDeposit = true;
    else if (a === "--verify-derive") out.verifyDeriveOnly = true;
    else if (a === "--token") out.token = argv[++i];
    else if (a === "--to") out.to = argv[++i];
    else if (a === "--amount") out.amountStr = argv[++i];
    else if (a === "--wei") out.weiStr = argv[++i];
    else if (a === "--decimals") out.decimals = Number(argv[++i]);
    else if (a === "--help" || a === "-h") out.help = true;
    else throw new Error(`Unknown argument: ${a}`);
  }
  return out;
}

function printHelp() {
  console.log(`transfer-erc20.mjs — ERC-20 transfer using POLYMARKET_PK + POLYGON_RPC_URL

Usage:
  node scripts/transfer-erc20.mjs --token <address> --to <address> [options]

Options:
  --from-deposit     Send from Polymarket deposit wallet (relayer WALLET batch; needs relayer API env)
  --amount <string>  Human amount (e.g. 12.5); omit to send full balance
  --wei <uint>       Raw amount in wei/smallest units (overrides --amount)
  --decimals <n>     Token decimals for --amount (default: read from contract, fallback 6)
  --verify-derive    Only run CREATE2 deposit-wallet derivation self-check and exit

Environment:
  POLYMARKET_PK, POLYGON_RPC_URL
  With --from-deposit: POLYMARKET_RELAYER_API_KEY, POLYMARKET_RELAYER_API_KEY_ADDRESS
`);
}

function normalizeBase(url) {
  const u = url.trim();
  return u.endsWith("/") ? u.slice(0, -1) : u;
}

async function relayerWalletNonce(baseUrl, owner) {
  const u = `${normalizeBase(baseUrl)}/nonce?address=${owner}&type=WALLET`;
  const r = await fetch(u);
  const t = await r.text();
  if (!r.ok) throw new Error(`relayer GET /nonce: ${r.status} ${t.trim()}`);
  const j = JSON.parse(t);
  return j.nonce;
}

async function waitRelayerTx(baseUrl, transactionId) {
  const base = normalizeBase(baseUrl);
  const maxMs = 240_000;
  const step = 2000;
  const end = Date.now() + maxMs;
  while (Date.now() < end) {
    const r = await fetch(`${base}/transaction?id=${encodeURIComponent(transactionId)}`);
    const t = await r.text();
    if (!r.ok) throw new Error(`relayer GET /transaction: ${r.status} ${t.trim()}`);
    const rows = JSON.parse(t);
    const row = Array.isArray(rows) ? rows[0] : rows;
    if (!row) {
      await new Promise((res) => setTimeout(res, step));
      continue;
    }
    if (row.state === "STATE_MINED" || row.state === "STATE_CONFIRMED") return row;
    if (row.state === "STATE_FAILED" || row.state === "STATE_INVALID") {
      throw new Error(`relayer tx ${transactionId} failed: ${row.state}`);
    }
    await new Promise((res) => setTimeout(res, step));
  }
  throw new Error(`relayer tx ${transactionId} timed out`);
}

async function transferFromDeposit({
  rpcUrl,
  privateKey,
  token,
  recipient,
  amount,
}) {
  const relKey = process.env.POLYMARKET_RELAYER_API_KEY?.trim();
  const relAddrStr = process.env.POLYMARKET_RELAYER_API_KEY_ADDRESS?.trim();
  const relayerUrl =
    process.env.RELAYER_URL?.trim() || "https://relayer-v2.polymarket.com";

  if (!relKey) throw new Error("POLYMARKET_RELAYER_API_KEY is required for --from-deposit");
  if (!relAddrStr)
    throw new Error("POLYMARKET_RELAYER_API_KEY_ADDRESS is required for --from-deposit");

  const account = privateKeyToAccount(normalizePrivateKey(privateKey));
  const owner = account.address;
  const depositWallet = deriveDepositWallet(
    owner,
    DEPOSIT_WALLET_FACTORY_POLYGON,
    DEPOSIT_WALLET_IMPLEMENTATION_POLYGON,
  );

  const publicClient = createPublicClient({
    chain: polygon,
    transport: http(rpcUrl),
  });

  const tokenAddr = getAddress(token);
  const toAddr = getAddress(recipient);

  const data = encodeFunctionData({
    abi: erc20Abi,
    functionName: "transfer",
    args: [toAddr, amount],
  });

  const nonce = await relayerWalletNonce(relayerUrl, owner);
  const deadline = BigInt(Math.floor(Date.now() / 1000) + 240);

  const calls = [
    {
      target: tokenAddr,
      value: 0n,
      data,
    },
  ];

  const signature = await account.signTypedData({
    domain: {
      name: "DepositWallet",
      version: "1",
      chainId: POLYGON_CHAIN_ID,
      verifyingContract: depositWallet,
    },
    types: {
      Call: [
        { name: "target", type: "address" },
        { name: "value", type: "uint256" },
        { name: "data", type: "bytes" },
      ],
      Batch: [
        { name: "wallet", type: "address" },
        { name: "nonce", type: "uint256" },
        { name: "deadline", type: "uint256" },
        { name: "calls", type: "Call[]" },
      ],
    },
    primaryType: "Batch",
    message: {
      wallet: depositWallet,
      nonce: BigInt(nonce),
      deadline,
      calls: calls.map((c) => ({
        target: c.target,
        value: c.value,
        data: c.data,
      })),
    },
  });

  const body = {
    type: "WALLET",
    from: owner,
    to: DEPOSIT_WALLET_FACTORY_POLYGON,
    nonce,
    signature,
    depositWalletParams: {
      depositWallet,
      deadline: deadline.toString(),
      calls: calls.map((c) => ({
        target: c.target,
        value: "0",
        data: c.data,
      })),
    },
  };

  const submitUrl = `${normalizeBase(relayerUrl)}/submit`;
  const resp = await fetch(submitUrl, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      RELAYER_API_KEY: relKey,
      RELAYER_API_KEY_ADDRESS: getAddress(relAddrStr),
    },
    body: JSON.stringify(body),
  });
  const txt = await resp.text();
  if (!resp.ok) throw new Error(`relayer POST /submit: ${resp.status} ${txt.trim()}`);
  const submitted = JSON.parse(txt);
  const txId = submitted.transactionID ?? submitted.transactionId;
  if (!txId) throw new Error(`unexpected /submit response: ${txt}`);

  console.error(`relayer transaction id: ${txId}`);
  const finalRow = await waitRelayerTx(relayerUrl, txId);
  const chainHash = finalRow.transactionHash ?? finalRow.transaction_hash;
  console.log(
    JSON.stringify(
      {
        mode: "deposit-wallet",
        transactionId: txId,
        state: finalRow.state,
        transactionHash: chainHash,
      },
      null,
      2,
    ),
  );

  if (chainHash) {
    await publicClient.waitForTransactionReceipt({ hash: chainHash }).catch(() => {});
  }
}

async function resolveAmount(publicClient, tokenAddr, holder, args) {
  if (args.weiStr != null) return BigInt(args.weiStr);
  let decimals = args.decimals;
  if (decimals == null || Number.isNaN(decimals)) {
    try {
      decimals = await publicClient.readContract({
        address: tokenAddr,
        abi: erc20Abi,
        functionName: "decimals",
      });
    } catch {
      decimals = 6;
    }
  }
  if (args.amountStr != null) {
    return parseUnits(args.amountStr, decimals);
  }
  const bal = await publicClient.readContract({
    address: tokenAddr,
    abi: erc20Abi,
    functionName: "balanceOf",
    args: [holder],
  });
  if (bal === 0n) throw new Error("balance is zero; nothing to send");
  return bal;
}

async function transferFromEoa({ rpcUrl, privateKey, token, recipient, args }) {
  const account = privateKeyToAccount(normalizePrivateKey(privateKey));
  const publicClient = createPublicClient({
    chain: polygon,
    transport: http(rpcUrl),
  });
  const walletClient = createWalletClient({
    account,
    chain: polygon,
    transport: http(rpcUrl),
  });

  const tokenAddr = getAddress(token);
  const toAddr = getAddress(recipient);
  const amount = await resolveAmount(publicClient, tokenAddr, account.address, args);

  const hash = await walletClient.writeContract({
    address: tokenAddr,
    abi: erc20Abi,
    functionName: "transfer",
    args: [toAddr, amount],
  });

  console.log(
    JSON.stringify(
      {
        mode: "eoa",
        from: account.address,
        transactionHash: hash,
      },
      null,
      2,
    ),
  );

  const receipt = await publicClient.waitForTransactionReceipt({ hash });
  console.error(`mined in block ${receipt.blockNumber}, status ${receipt.status}`);
}

async function main() {
  const argv = parseArgs(process.argv);
  if (argv.help) {
    printHelp();
    process.exit(0);
  }

  const got = deriveDepositWallet(
    DERIVE_TEST_OWNER,
    DEPOSIT_WALLET_FACTORY_POLYGON,
    DEPOSIT_WALLET_IMPLEMENTATION_POLYGON,
  );
  if (got.toLowerCase() !== DERIVE_TEST_EXPECTED.toLowerCase()) {
    throw new Error(`derive self-check failed: got ${got}, expected ${DERIVE_TEST_EXPECTED}`);
  }

  if (argv.verifyDeriveOnly) {
    console.log("deriveDepositWallet self-check OK");
    process.exit(0);
  }

  const rpcUrl = process.env.POLYGON_RPC_URL?.trim();
  if (!rpcUrl) throw new Error("POLYGON_RPC_URL is required");

  const pk = process.env.POLYMARKET_PK;
  if (!argv.token || !argv.to) {
    printHelp();
    throw new Error("--token and --to are required (unless --verify-derive)");
  }
  if (!isAddress(argv.token)) throw new Error("invalid --token address");
  if (!isAddress(argv.to)) throw new Error("invalid --to address");

  if (argv.fromDeposit) {
    const account = privateKeyToAccount(normalizePrivateKey(pk));
    const publicClient = createPublicClient({
      chain: polygon,
      transport: http(rpcUrl),
    });
    const depositWallet = deriveDepositWallet(
      account.address,
      DEPOSIT_WALLET_FACTORY_POLYGON,
      DEPOSIT_WALLET_IMPLEMENTATION_POLYGON,
    );
    const tokenAddr = getAddress(argv.token);
    const amount = await resolveAmount(publicClient, tokenAddr, depositWallet, argv);
    await transferFromDeposit({
      rpcUrl,
      privateKey: pk,
      token: argv.token,
      recipient: argv.to,
      amount,
    });
  } else {
    await transferFromEoa({
      rpcUrl,
      privateKey: pk,
      token: argv.token,
      recipient: argv.to,
      args: argv,
    });
  }
}

main().catch((e) => {
  console.error(e.message || e);
  process.exit(1);
});
