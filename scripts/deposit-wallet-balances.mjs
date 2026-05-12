#!/usr/bin/env node
/**
 * Poll Polymarket **deposit wallet** (CREATE2 proxy) **Cash** and **Claimable** USDC-style totals.
 *
 * Mirrors `src/balances.rs` + Data API redeemable positions (same as the TUI balance panel).
 *
 * Env (repo-root `.env` when run from project root):
 *   POLYMARKET_PK   — 0x-prefixed 32-byte hex (owner EOA; deposit wallet is derived from this)
 *   POLYGON_RPC_URL — Polygon JSON-RPC for Multicall3 / CTF reads
 *
 * Optional:
 *   POLYMARKET_PROXY — forwarded to Data API `fetch` (http/https/socks5 URL)
 *
 * Usage:
 *   node scripts/deposit-wallet-balances.mjs
 *   node scripts/deposit-wallet-balances.mjs --interval-ms 3000 --once
 */

import "dotenv/config";
import {
  concat,
  decodeFunctionResult,
  encodeAbiParameters,
  encodeFunctionData,
  getAddress,
  getCreate2Address,
  http,
  keccak256,
  pad,
  toHex,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { createPublicClient } from "viem";
import { polygon } from "viem/chains";

const DATA_API_HOST = "https://data-api.polymarket.com";
const POSITIONS_PAGE = 500;
const POSITIONS_MAX_OFFSET = 10_000;
const MAX_AGGREGATE3_CALLS = 256;

const USDC_E = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const PUSD = "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB";
/** EIP-55 checksum (viem rejects `...cF65f87097...`). */
const CTF = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11";

const DEPOSIT_WALLET_FACTORY_POLYGON = "0x00000000000Fb5C9ADea0298D729A0CB3823Cc07";
const DEPOSIT_WALLET_IMPLEMENTATION_POLYGON =
  "0x58CA52ebe0DadfdF531Cde7062e76746de4Db1eB";

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

/** Same as `scripts/transfer-erc20.mjs` / `src/deposit_wallet.rs`. */
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
];

const ctfAbi = [
  {
    type: "function",
    name: "payoutDenominator",
    stateMutability: "view",
    inputs: [{ name: "conditionId", type: "bytes32" }],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "payoutNumerators",
    stateMutability: "view",
    inputs: [
      { name: "conditionId", type: "bytes32" },
      { name: "index", type: "uint256" },
    ],
    outputs: [{ type: "uint256" }],
  },
  {
    type: "function",
    name: "balanceOf",
    stateMutability: "view",
    inputs: [
      { name: "account", type: "address" },
      { name: "id", type: "uint256" },
    ],
    outputs: [{ type: "uint256" }],
  },
];

const multicall3Abi = [
  {
    type: "function",
    name: "aggregate3",
    stateMutability: "payable",
    inputs: [
      {
        name: "calls",
        type: "tuple[]",
        components: [
          { name: "target", type: "address" },
          { name: "allowFailure", type: "bool" },
          { name: "callData", type: "bytes" },
        ],
      },
    ],
    outputs: [
      {
        name: "returnData",
        type: "tuple[]",
        components: [
          { name: "success", type: "bool" },
          { name: "returnData", type: "bytes" },
        ],
      },
    ],
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

function parseConditionId(s) {
  const t = String(s).trim();
  const h = t.startsWith("0x") ? t.slice(2) : t;
  if (h.length !== 64) throw new Error(`conditionId must be 32 bytes hex, got ${h.length / 2} bytes`);
  return `0x${h}`;
}

function parseTokenIdU256(s) {
  const t = String(s).trim();
  if (t.startsWith("0x") || t.startsWith("0X")) {
    return BigInt(t);
  }
  return BigInt(t);
}

function parentCollectionId() {
  return pad("0x0", { size: 32 });
}

function collectionId(conditionId, indexSet) {
  const idx = pad(toHex(BigInt(indexSet)), { size: 32 });
  return keccak256(concat([parentCollectionId(), conditionId, idx]));
}

function positionId(collateral, collectionIdBytes32) {
  const h = keccak256(concat([getAddress(collateral), collectionIdBytes32]));
  return BigInt(h);
}

function conditionPositionIds(conditionId) {
  const c1 = collectionId(conditionId, 1);
  const c2 = collectionId(conditionId, 2);
  const ids = {
    pos1Usdc: positionId(USDC_E, c1),
    pos2Usdc: positionId(USDC_E, c2),
    pos1Pusd: positionId(PUSD, c1),
    pos2Pusd: positionId(PUSD, c2),
  };
  return {
    ...ids,
    contains(tid) {
      const t = BigInt(tid);
      return (
        t === ids.pos1Usdc ||
        t === ids.pos2Usdc ||
        t === ids.pos1Pusd ||
        t === ids.pos2Pusd
      );
    },
    isOutcomeOne(tid) {
      const t = BigInt(tid);
      return t === ids.pos1Usdc || t === ids.pos1Pusd;
    },
  };
}

function u256ToUsdc6(n) {
  return Number(n) / 1_000_000;
}

function decodeUint256Return(data) {
  const hex = typeof data === "string" ? data : data;
  if (!hex || hex === "0x") return 0n;
  const slice = hex.length >= 66 ? `0x${hex.slice(-64)}` : hex;
  return BigInt(slice);
}

function prepareStandardClaimableParsed(rows) {
  /** @type {Map<string, { cid: `0x${string}`; tid: bigint; v: number }>} */
  const byKey = new Map();
  for (const p of rows) {
    if (!p.redeemable || p.negativeRisk) continue;
    let cid;
    let tid;
    try {
      cid = parseConditionId(p.conditionId);
      tid = parseTokenIdU256(p.asset);
    } catch {
      continue;
    }
    const cv = p.currentValue;
    if (!Number.isFinite(cv)) continue;
    const key = `${cid}:${tid.toString()}`;
    const prev = byKey.get(key);
    if (!prev || cv > prev.v) byKey.set(key, { cid, tid, v: cv });
  }
  const parsed = [...byKey.values()];
  if (parsed.length === 0) {
    return { parsed: [], conds: [], tokenIds: [] };
  }
  const condSet = new Set(parsed.map((x) => x.cid));
  const conds = [...condSet].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  const tidSet = new Set(parsed.map((x) => x.tid));
  const tokenIds = [...tidSet].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  return { parsed, conds, tokenIds };
}

function claimableStandardTotalFromMaps(parsed, payouts, balances) {
  let total = 0;
  /** @type {Map<string, ReturnType<typeof conditionPositionIds>>} */
  const posIdsByCond = new Map();
  for (const { cid, tid, v: apiFb } of parsed) {
    const payout = payouts.get(cid);
    if (!payout) {
      total += apiFb;
      continue;
    }
    const [d, n0, n1] = payout;
    let ids = posIdsByCond.get(cid);
    if (!ids) {
      ids = conditionPositionIds(cid);
      posIdsByCond.set(cid, ids);
    }
    const b = balances.get(tid.toString()) ?? 0n;
    if (d === 0n) {
      total += apiFb;
      continue;
    }
    if (!ids.contains(tid)) {
      total += apiFb;
      continue;
    }
    const n = ids.isOutcomeOne(tid) ? n0 : n1;
    const slotUsdc = u256ToUsdc6((b * n) / d);
    if (slotUsdc > 1e-9) total += slotUsdc;
    else if (apiFb > 1e-9) total += apiFb;
  }
  return total;
}

function sumNegRiskClaimableUsdc(rows) {
  return rows
    .filter((p) => p.redeemable && p.negativeRisk)
    .map((p) => p.currentValue)
    .filter((v) => Number.isFinite(v))
    .reduce((a, b) => a + b, 0);
}

function sumClaimableUsdcApi(rows) {
  return rows
    .filter((p) => p.redeemable)
    .map((p) => p.currentValue)
    .filter((v) => Number.isFinite(v))
    .reduce((a, b) => a + b, 0);
}

async function fetchRedeemablePositions(userAddress) {
  const user = getAddress(userAddress).toLowerCase();
  const out = [];
  let offset = 0;
  for (;;) {
    const url = `${DATA_API_HOST}/positions?user=${user}&limit=${POSITIONS_PAGE}&offset=${offset}&redeemable=true&sizeThreshold=0`;
    const r = await fetch(url, { headers: dataApiHeaders() });
    const txt = await r.text();
    if (!r.ok) throw new Error(`Data API GET /positions: ${r.status} ${txt.trim().slice(0, 200)}`);
    const batch = JSON.parse(txt);
    const n = batch.length;
    out.push(...batch);
    if (n < POSITIONS_PAGE) break;
    offset += POSITIONS_PAGE;
    if (offset > POSITIONS_MAX_OFFSET) break;
  }
  return out;
}

function dataApiHeaders() {
  const proxy = process.env.POLYMARKET_PROXY?.trim();
  if (!proxy) return {};
  // Node fetch does not read POLYMARKET_PROXY; document that users can set global HTTPS_PROXY
  // or run behind a system VPN. Optional hint only.
  return {};
}

async function rpcAggregate3Chunk(publicClient, calls) {
  const data = encodeFunctionData({
    abi: multicall3Abi,
    functionName: "aggregate3",
    args: [calls],
  });
  const raw = await publicClient.call({
    to: MULTICALL3,
    data,
  });
  return decodeFunctionResult({
    abi: multicall3Abi,
    functionName: "aggregate3",
    data: raw.data,
  });
}

/**
 * @param {import('viem').PublicClient} publicClient
 * @param {`0x${string}`} funder
 * @param {any[]} redeemableRows
 */
async function fetchOnchainCashAndClaimStd(publicClient, funder, redeemableRows) {
  const { parsed, conds, tokenIds } = prepareStandardClaimableParsed(redeemableRows);
  if (parsed.length === 0) {
    const usdc = await readErc20Balance(publicClient, USDC_E, funder);
    const pusd = await readErc20Balance(publicClient, PUSD, funder);
    return { cash: usdc + pusd, claimStd: 0 };
  }

  /** @type {{ target: `0x${string}`; allowFailure: boolean; callData: `0x${string}` }[]} */
  const callList = [];
  callList.push({
    target: USDC_E,
    allowFailure: true,
    callData: encodeFunctionData({
      abi: erc20Abi,
      functionName: "balanceOf",
      args: [funder],
    }),
  });
  callList.push({
    target: PUSD,
    allowFailure: true,
    callData: encodeFunctionData({
      abi: erc20Abi,
      functionName: "balanceOf",
      args: [funder],
    }),
  });
  for (const cond of conds) {
    callList.push({
      target: CTF,
      allowFailure: true,
      callData: encodeFunctionData({
        abi: ctfAbi,
        functionName: "payoutDenominator",
        args: [cond],
      }),
    });
    callList.push({
      target: CTF,
      allowFailure: true,
      callData: encodeFunctionData({
        abi: ctfAbi,
        functionName: "payoutNumerators",
        args: [cond, 0n],
      }),
    });
    callList.push({
      target: CTF,
      allowFailure: true,
      callData: encodeFunctionData({
        abi: ctfAbi,
        functionName: "payoutNumerators",
        args: [cond, 1n],
      }),
    });
  }
  for (const tid of tokenIds) {
    callList.push({
      target: CTF,
      allowFailure: true,
      callData: encodeFunctionData({
        abi: ctfAbi,
        functionName: "balanceOf",
        args: [funder, tid],
      }),
    });
  }

  /** @type {{ success: boolean; returnData: `0x${string}` }[]} */
  let results = [];
  for (let i = 0; i < callList.length; i += MAX_AGGREGATE3_CALLS) {
    const chunk = callList.slice(i, i + MAX_AGGREGATE3_CALLS);
    const part = await rpcAggregate3Chunk(publicClient, chunk);
    results = results.concat(part);
  }

  let idx = 0;
  const usdcOk = results[idx].success;
  const usdcRaw = results[idx].returnData;
  idx += 1;
  const pusdOk = results[idx].success;
  const pusdRaw = results[idx].returnData;
  idx += 1;
  const usdcAmt = usdcOk ? u256ToUsdc6(decodeUint256Return(usdcRaw)) : 0;
  const pusdAmt = pusdOk ? u256ToUsdc6(decodeUint256Return(pusdRaw)) : 0;
  const cash = usdcAmt + pusdAmt;

  /** @type {Map<string, [bigint, bigint, bigint]>} */
  const payouts = new Map();
  for (const cond of conds) {
    const d = results[idx].success ? decodeUint256Return(results[idx].returnData) : 0n;
    idx += 1;
    const n0 = results[idx].success ? decodeUint256Return(results[idx].returnData) : 0n;
    idx += 1;
    const n1 = results[idx].success ? decodeUint256Return(results[idx].returnData) : 0n;
    idx += 1;
    payouts.set(cond, [d, n0, n1]);
  }

  /** @type {Map<string, bigint>} */
  const balances = new Map();
  for (const tid of tokenIds) {
    const b = results[idx].success ? decodeUint256Return(results[idx].returnData) : 0n;
    idx += 1;
    balances.set(tid.toString(), b);
  }

  const claimStd = claimableStandardTotalFromMaps(parsed, payouts, balances);
  return { cash, claimStd };
}

async function readErc20Balance(publicClient, token, holder) {
  const raw = await publicClient.readContract({
    address: token,
    abi: erc20Abi,
    functionName: "balanceOf",
    args: [holder],
  });
  return u256ToUsdc6(raw);
}

async function fetchBalancePanelUsdc(publicClient, funder) {
  let redeemableRows = [];
  try {
    redeemableRows = await fetchRedeemablePositions(funder);
  } catch (e) {
    console.warn(`Data API redeemable positions failed — claimable may be understated: ${e.message}`);
  }

  const claimNeg = sumNegRiskClaimableUsdc(redeemableRows);
  const { cash, claimStd } = await fetchOnchainCashAndClaimStd(
    publicClient,
    funder,
    redeemableRows,
  );
  const apiTotal = sumClaimableUsdcApi(redeemableRows);
  const totalClaim = Math.max(claimStd + claimNeg, apiTotal);
  return { cash, claimable: totalClaim };
}

function parseArgs(argv) {
  let intervalMs = 5000;
  let once = false;
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--once") once = true;
    else if (a === "--interval-ms") intervalMs = Number(argv[++i]);
    else if (a === "--help" || a === "-h") return { help: true, intervalMs, once };
    else throw new Error(`Unknown argument: ${a}`);
  }
  if (!Number.isFinite(intervalMs) || intervalMs < 500) {
    throw new Error("--interval-ms must be a number >= 500");
  }
  return { help: false, intervalMs, once };
}

function printHelp() {
  console.log(`deposit-wallet-balances.mjs — live Cash + Claimable for Polymarket deposit wallet

Derives the deposit wallet from POLYMARKET_PK (same CREATE2 as transfer-erc20.mjs), then:
  Cash      — USDC.e + pUSD on that address
  Claimable — on-chain CTF standard + neg-risk Data API (same max() rule as src/balances.rs)

Environment:
  POLYMARKET_PK, POLYGON_RPC_URL

Options:
  --interval-ms <n>   Refresh interval in ms (default: 5000)
  --once                Single fetch and exit
`);
}

function assertDeriveSelfCheck() {
  const got = deriveDepositWallet(
    DERIVE_TEST_OWNER,
    DEPOSIT_WALLET_FACTORY_POLYGON,
    DEPOSIT_WALLET_IMPLEMENTATION_POLYGON,
  );
  if (got.toLowerCase() !== DERIVE_TEST_EXPECTED.toLowerCase()) {
    throw new Error(`derive self-check failed: got ${got}, expected ${DERIVE_TEST_EXPECTED}`);
  }
}

function fmtUsd(v) {
  if (!Number.isFinite(v)) return "$—";
  return `$${v.toFixed(2)}`;
}

async function main() {
  const argv = parseArgs(process.argv);
  if (argv.help) {
    printHelp();
    process.exit(0);
  }

  assertDeriveSelfCheck();

  const rpcUrl = process.env.POLYGON_RPC_URL?.trim();
  if (!rpcUrl) throw new Error("POLYGON_RPC_URL is required");

  const account = privateKeyToAccount(normalizePrivateKey(process.env.POLYMARKET_PK));
  const owner = account.address;
  const depositWallet = getAddress(
    deriveDepositWallet(owner, DEPOSIT_WALLET_FACTORY_POLYGON, DEPOSIT_WALLET_IMPLEMENTATION_POLYGON),
  );

  const publicClient = createPublicClient({
    chain: polygon,
    transport: http(rpcUrl),
  });

  console.log(`EOA (signer):     ${owner}`);
  console.log(`Deposit wallet:   ${depositWallet}`);
  if (process.env.POLYMARKET_PROXY?.trim()) {
    console.warn(
      "POLYMARKET_PROXY is set — Node fetch may still bypass it; use a system proxy/VPN if Data API is geo-blocked.",
    );
  }

  const poll = async () => {
    const t = new Date().toISOString();
    try {
      const { cash, claimable } = await fetchBalancePanelUsdc(publicClient, depositWallet);
      process.stdout.write(`\r\x1b[K[${t}] Cash ${fmtUsd(cash)}  |  Claimable ${fmtUsd(claimable)}`);
      if (argv.once) console.log();
    } catch (e) {
      process.stdout.write(`\r\x1b[K[${t}] error: ${e.message || e}`);
      if (argv.once) console.log();
    }
  };

  await poll();
  if (argv.once) return;

  setInterval(poll, argv.intervalMs);
}

main().catch((e) => {
  console.error(e.message || e);
  process.exit(1);
});
