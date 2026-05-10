# polymarket-crypto

A terminal trading interface for Polymarket's rolling **Up or Down** crypto prediction markets (BTC, ETH, SOL, XRP), built in Rust with a full TUI.

---

> ⚠️ **Educational project — use at your own risk.**
> This software is developed for learning purposes. It interacts with real money on a live prediction market. There is no warranty of any kind. You are solely responsible for any funds lost due to bugs, misconfiguration, or unexpected market behavior. Start with very small amounts and verify every action before trusting it with real funds.

---

## What it does

Polymarket runs short-window crypto prediction markets: every 5 or 15 minutes a new "Up or Down?" market opens for BTC, ETH, SOL, or XRP. You bet on whether the price will be higher or lower than the opening price when the window closes.

This terminal app lets you:

- Watch the **live spot price** (Chainlink) vs each window's **Price to Beat**
- See the full **UP / DOWN order book** in real time
- Place **market orders** (FAK — Fill or Kill) and **limit orders** (GTD — expire just before window close) with single keypresses
- Track your **open positions and unrealized PnL**
- Use an optional **trailing stop** — tracks **best bid**, arms after a gross move (`TAKE_PROFIT_BPS`), then FAK-sells when the trail trips; works for **market (FAK) and limit (GTD) buys** (resting fills arm via the user-channel when you are **maker** on the trade)
- View **sentiment** (CLOB mid-price or top holders from Data API)
- **Redeem** resolved winnings via the Polymarket relayer (Safe wallets)
- Fetch a **Solana USDC deposit address** via Polymarket Bridge (key `f`)

There is also an optional **Node.js script** [`scripts/transfer-erc20.mjs`](scripts/transfer-erc20.mjs) to send ERC-20 tokens from your EOA or (with the relayer) from your Polymarket **deposit wallet** — see [Scripts — ERC-20 transfer](#scripts--erc-20-transfer).

Everything happens in the terminal. No browser required once you have your wallet configured.

![Terminal trading interface: spot price, UP/DOWN order book, balances, positions, and key bindings](screenshot.png)

## Quick start

### Option A — pre-built binary (no Rust required)

1. **Download** the binary for your platform from the [Releases page](../../releases/latest):

   | Platform | File |
   |---|---|
   | Linux x86-64 | `polymarket-crypto-linux-x86_64` |
   | macOS Apple Silicon (M1/M2/M3) | `polymarket-crypto-macos-aarch64` |
   | Windows x86-64 | `polymarket-crypto-windows-x86_64.exe` |

   Also grab **`.env.example`** from the repository root — it is not bundled in the release. Download it from the repo's file list on GitHub.

2. **Create your `.env`** from the example:

   **Linux / macOS**
   ```sh
   cp .env.example .env
   ```
   **Windows (cmd)**
   ```cmd
   copy .env.example .env
   ```
   **Windows (PowerShell)**
   ```powershell
   Copy-Item .env.example .env
   ```

3. **Edit `.env`** — fill in at minimum `POLYMARKET_PK`, `POLYMARKET_FUNDER`, and `POLYGON_RPC_URL` (see [Configure `.env`](#configure-env) below).

4. **Run**:

   **Linux**
   ```sh
   chmod +x polymarket-crypto-linux-x86_64
   ./polymarket-crypto-linux-x86_64
   ```

   **macOS** (Apple Silicon only — M1/M2/M3)
   ```sh
   chmod +x polymarket-crypto-macos-aarch64
   # Remove the quarantine flag set by the browser download
   xattr -dr com.apple.quarantine ./polymarket-crypto-macos-aarch64
   ./polymarket-crypto-macos-aarch64
   ```
   If macOS still blocks launch, open **System Settings → Privacy & Security** and click **Open Anyway** next to the blocked binary.

   **Windows**
   ```powershell
   # Run from the same directory as your .env file
   .\polymarket-crypto-windows-x86_64.exe
   ```
   If Windows Defender SmartScreen blocks the binary, click **More info → Run anyway**.

---

### Option B — build from source

#### Prerequisites

- **Rust 1.80+** — install via [rustup](https://rustup.rs)
- A **funded Polymarket wallet**. The typical setup is an EOA (private key) that controls a Gnosis Safe holding your USDC. Both addresses go in `.env`.
- The Safe must have approved the CTF Exchange and NegRisk Exchange as spenders. If you've ever placed a trade through the Polymarket web UI, this is already done.
- A **Polygon RPC endpoint** (Alchemy, drpc, or the free `polygon-rpc.com`) for on-chain balance reads.

#### Build

```sh
git clone <this-repo>
cd polymarket-crypto
cp .env.example .env
# Edit .env — see configuration section below
cargo build --release
```

### Configure `.env`

The only required fields to start:

| Variable | What it is |
|---|---|
| `POLYMARKET_PK` | Private key of the EOA that signs orders (starts with `0x`) |
| `POLYMARKET_FUNDER` | Address that holds your USDC — your Gnosis Safe address for most users |
| `POLYMARKET_SIG_TYPE` | `2` Gnosis Safe (default), `0` plain EOA, `3` deposit wallet ([`POLY_1271`](https://docs.polymarket.com/trading/deposit-wallet-migration)) |
| `POLYGON_RPC_URL` | Polygon HTTPS endpoint for balance reads |
| `DEFAULT_SIZE_USDC` | Default order size in USDC (e.g. `5.0`) |
| `DEFAULT_PRICE` | Default limit price for `[`/`]` quick-limit keys and the `l` modal pre-fill (0.01–0.99, default `0.50`) |

Optional but useful:

| Variable | What it is |
|---|---|
| `POLYMARKET_PROXY` | Proxy for geo-blocked regions — see [Geo-restrictions](#geo-restricted) |
| `AUTOTRADING` | `true` enables automatic **market FAK BUY** entries from `STRONG UP` / `STRONG DOWN` strategy signals only (`false` by default) |
| `AUTOTRADING_MAX_POSITIONS` | Maximum currently open auto-trading positions; sold/closed auto inventory frees capacity (`1` by default) |
| `BUY_TRAIL_BPS` | Trailing stop width in bps from peak best bid (`0` = off). Applies after **market or limit** buys that arm the trail. Legacy: `MARKET_BUY_TRAIL_BPS` is used if `BUY_TRAIL_BPS` is unset |
| `TAKE_PROFIT_BPS` | **Activation**: trail arms when `best_bid ≥ entry × (1 + bps/10_000)` (`0` ≈ arm as soon as bid reaches entry). With `BUY_TRAIL_BPS` at `0`, also places an auto **GTD take-profit sell** after a **market or limit** buy fills (POST fill or maker fill on user WebSocket). Legacy: `MARKET_BUY_TAKE_PROFIT_BPS` is used if `TAKE_PROFIT_BPS` is unset |
| `MARKET_BUY_SLIPPAGE_BPS` | Slippage cushion for market buys. Default `50` (0.5%) |
| `POLYMARKET_RELAYER_API_KEY` | Redeem (`x` / `X`, Safe only), **`deploy-wallet`**, and deposit-wallet **approvals** (`b` in TUI when `SIG_TYPE=3`) — same Relayer API key from Settings → API |

### Deposit wallet (`POLYMARKET_SIG_TYPE=3`)

Polymarket’s **[deposit wallet](https://docs.polymarket.com/trading/deposit-wallet-migration)** flow uses on-chain **`POLY_1271`** signatures and a deterministic proxy address derived from your EOA.

1. Deploy `WALLET-CREATE` once: **`polymarket-crypto deploy-wallet`**. Needs **`POLYMARKET_RELAYER_API_KEY`** + **`POLYMARKET_RELAYER_API_KEY_ADDRESS`** (same as CTF redeem). **`RELAYER_URL`** defaults to production; set it for preprod.

   ```sh
   polymarket-crypto deploy-wallet
   ```

   Then set **`POLYMARKET_FUNDER`** to the printed deposit wallet address (the bot checks it matches the deterministic address in [`src/deposit_wallet.rs`](src/deposit_wallet.rs) for your `POLYMARKET_PK`).

2. **Approvals** — in the TUI, press **`b`** to submit a relayer **`WALLET`** batch: pUSD `approve` + CTF `setApprovalForAll` for CTF Exchange V2 and Neg-risk Exchange V2 (same relayer API key as `deploy-wallet` / redeem). Then call CLOB balance sync as in the [migration guide](https://docs.polymarket.com/trading/deposit-wallet-migration) (the app refreshes collateral + current market conditional allowances after a successful batch).
3. Fund **pUSD** to the deposit wallet; EOA balance does not count as CLOB buying power for this mode.
4. `x` / **CTF redeem** in this app still targets **Gnosis Safe** (`POLYMARKET_SIG_TYPE=2`) only; deposit-wallet redeem needs `WALLET` batches (not implemented here).

See [`.env.example`](.env.example) for the full list and descriptions.

#### Run (after build from source)

```sh
./target/release/polymarket-crypto
```

On launch, a wizard appears:

1. **Pick an asset** — BTC, ETH, SOL, or XRP (↑/↓ or `j`/`k`, then Enter)
2. **Pick a timeframe** — 5m or 15m (↑/↓ or `j`/`k`, then Enter)

You'll then land on the trading screen. Press **`q`** or **Ctrl-C** to exit; **`Esc`** goes back to the timeframe picker.

## Key bindings

### Trading screen

| Key | Action |
|---|---|
| `w` / `s` | Market **BUY** UP / DOWN (FAK at best ask) |
| `a` / `d` | Market **SELL** UP / DOWN (FAK at best bid) |
| `l` | Open limit-order modal (pre-fills default price) |
| `[` | Quick limit **BUY UP** GTD — default size & price |
| `]` | Quick limit **BUY DOWN** GTD — default size & price |
| `c` | Cancel ALL open orders |
| `x` / `X` | Redeem all resolved positions (requires relayer key) |
| `e` | Edit default ticket size |
| `p` | Edit default limit price |
| `f` | Fetch Solana USDC deposit address + QR code |
| `r` | Force-refresh active market |
| `Esc` | Return to timeframe picker |
| `q` / Ctrl-C | Quit |

Holding a key down does **not** fire repeated orders — key repeat is ignored.

### Default size and price

The status bar shows two editable defaults used by quick-trade keys and as pre-filled values when opening the limit modal:

- **`e`** — edit **size** (USDC budget, shown in yellow)
- **`p`** — edit **price** (limit price 0.01–0.99, shown in cyan; default `0.50`)

Type digits and `.`, then `Enter` or `Esc` to confirm. Invalid input reverts to the previous value.

### Limit-order modal (`l`)

| Key | Action |
|---|---|
| `w` / `s` / `a` / `d` | Set outcome + side (UP/DOWN, BUY/SELL) |
| ← / → | Flip outcome (UP ↔ DOWN) |
| ↑ / ↓ | Flip side (BUY ↔ SELL) |
| `Tab` | Switch between price and size fields |
| digits / `.` | Edit current field |
| `Enter` | Submit GTD limit order |
| `Esc` | Close modal |

GTD orders expire one second before the active window closes. The app enforces a minimum of 5 shares on submit to avoid `INVALID_ORDER_MIN_SIZE` from the CLOB API.

### Order sizing

- **Market BUY** (`w`/`s`) — size is a **USDC budget** (Polymarket FAK = dollar amount to spend)
- **Market SELL** (`a`/`d`) — size is **outcome shares**
- **Limit BUY** — type the notional in USDC; the app converts to shares before submitting
- **Limit SELL** — type shares directly

### Autotrading

Set `AUTOTRADING=true` to let the app automatically submit **market FAK BUY** orders when the strategy signal is `STRONG UP` or `STRONG DOWN`. `WATCH` and `NO TRADE` never place automatic orders.

Autotrading uses the same size shown in the TUI as manual market buys: `DEFAULT_SIZE_USDC` at startup, then whatever you edit with **`e`** while the app is running. Before each retry, the bot re-checks that the same strong signal is still present and recomputes the FAK buy from the current book. `AUTOTRADING_MAX_POSITIONS` limits currently open auto-trading positions; when matching sell fills reduce the tracked auto inventory, capacity opens again.

Autotrading only buys. Exits are still handled by your existing `TAKE_PROFIT_BPS` / `BUY_TRAIL_BPS` settings or by manual sells.

## Safety tips

1. Start with a small `DEFAULT_SIZE_USDC` (e.g. `1.0` or `5.0`) until you trust the setup.
2. Run `debug-auth` before your first trade to verify credentials are correct (see [Troubleshooting](#troubleshooting-clob-credentials)).
3. If you enable `BUY_TRAIL_BPS`, a trip fires a real **FAK SELL** (market or limit buy). Test with minimum size first. For **resting limit buys**, the trail is registered when the fill hits the user WebSocket as a **maker** leg; an immediate limit match in the POST response uses the same path as a market buy.
4. If you enable `AUTOTRADING`, start with `AUTOTRADING_MAX_POSITIONS=1` and a very small `DEFAULT_SIZE_USDC`; each `STRONG` signal can place a real market FAK BUY.
5. Never commit `.env` with real keys. The example ships with zero-filled keys so a copy without editing will fail loudly.

## Geo-restricted?

Polymarket blocks a broad set of IPs. If the spot price, order book, and Gamma data are all missing, you're likely blocked. Set `POLYMARKET_PROXY` in `.env`:

```
POLYMARKET_PROXY=http://user:pass@proxy.example.com:8080   # HTTP/HTTPS
POLYMARKET_PROXY=socks5://127.0.0.1:1080                   # SOCKS5
```

All traffic — REST, WebSockets, Chainlink feed — tunnels through the proxy. TLS is end-to-end so the proxy only sees ciphertext. A residential or datacenter proxy in any non-blocked region works.

## Troubleshooting CLOB credentials

If the status line shows auth errors, run the diagnostic command (skip the TUI entirely):

```sh
# source build
./target/release/polymarket-crypto debug-auth
# pre-built binary — use the name for your platform:
./polymarket-crypto-linux-x86_64 debug-auth        # Linux
./polymarket-crypto-macos-aarch64 debug-auth        # macOS
.\polymarket-crypto-windows-x86_64.exe debug-auth  # Windows
```

It prints every intermediate signing value (typeHash, domainSeparator, digest, signature) and the exact HTTP status + body from Polymarket's auth endpoints.

Common failures:

| `debug-auth` output | Cause |
|---|---|
| `401` — mentions *signature* or *address* | `POLYMARKET_PK` doesn't match the wallet Polymarket expects |
| `401` — mentions *timestamp* | Clock skew > ~10 s. Run `sudo ntpdate pool.ntp.org` |
| `403 Forbidden` | Proxy region still blocked, or wallet tripped compliance |
| `404 Not Found` on derive, `200` on create | No existing API key — now created, will work on next launch |
| Warning: funder equals signer | Wrong `POLYMARKET_SIG_TYPE`. Use `0` for plain EOA, `2` for Safe |

**Wallet never used on polymarket.com?** Log in once, deposit a small amount, then re-run `debug-auth`. The backend needs a record of the wallet before it will issue API keys.

Every run also writes a full debug log to `./polymarket-crypto.log` (override with `POLYMARKET_CRYPTO_LOG_PATH`). `tail -f polymarket-crypto.log` in a second pane is useful while the TUI runs.

## Architecture overview

The app is event-driven: all producers send `AppEvent` messages over a bounded `mpsc` channel (512 slots). The main loop drains events in batches, updates `AppState`, and calls `ratatui::draw`. Redraws are throttled to ~20 Hz during feed-heavy bursts so the app doesn't pin a CPU core.

```
Keyboard / resize ──┐
1 Hz ticker ────────┤
Chainlink RTDS WS ──┤──▶  mpsc<AppEvent>  ──▶  AppState  ──▶  ratatui draw
CLOB book WS ───────┤
CLOB user WS ───────┤
Gamma poll (15s/1s) ┘
```

Network I/O never runs on the render path. Trading actions (place order, cancel, redeem, bridge) dispatch onto separate `tokio` tasks. Orders are signed with EIP-712 using `alloy` + `alloy-sol-types` and the on-chain `Order` shape from the CTF Exchange.

## Project layout

```
scripts/
├── package.json                 # Node deps for transfer-erc20.mjs (viem, dotenv)
└── transfer-erc20.mjs           # ERC-20 transfer helper (EOA or deposit wallet via relayer)
src/
├── main.rs                      # event loop, action dispatch, wizard
├── app.rs                       # AppState, positions, fills, event reducer
├── config.rs                    # env vars and endpoints
├── events.rs                    # keyboard → Action (pure)
├── market_profile.rs            # assets, timeframes, rolling slug helpers
├── trading.rs                   # EIP-712 orders, L1/L2 auth, CLOB REST
├── balances.rs                  # on-chain USDC + CTF claimable (Multicall3)
├── data_api.rs                  # positions, holders, redeemable markets
├── redeem.rs                    # CTF redeem via Polymarket relayer (Safe)
├── fees.rs                      # taker fee math, take-profit price
├── trailing_stop.rs             # lock-free trailing stop (hot path: best bid)
├── gamma.rs / gamma_series.rs   # Gamma REST, market discovery, GTD expiry
├── bridge_deposit.rs            # Solana USDC deposit address + QR
├── net.rs                       # proxy-aware HTTP + WebSocket connect
├── feeds/
│   ├── chainlink.rs             # RTDS WS → spot price
│   ├── clob_ws.rs               # per-market CLOB WS → order book
│   ├── clob_user_ws.rs          # authenticated user WS → fills, order hints
│   ├── user_trade_sync.rs       # trade replay / dedup
│   └── market_discovery_gamma.rs # Gamma poll + market roll
└── ui/render.rs                 # ratatui layout
```

## Scripts — ERC-20 transfer

[`scripts/transfer-erc20.mjs`](scripts/transfer-erc20.mjs) transfers an ERC-20 balance **from the wallet derived from `POLYMARKET_PK`** to any recipient. It loads `.env` from the current working directory (run from the repo root where `.env` lives).

**Prerequisites:** Node.js 18+ and a one-time install:

```sh
cd scripts && npm install
```

**Required environment variables:** `POLYMARKET_PK`, `POLYGON_RPC_URL`.

**Usage**

| Mode | Command summary |
|---|---|
| **EOA** (default) | Sends from the plain address derived from `POLYMARKET_PK`. You need POL on Polygon for gas. |
| **Deposit wallet** | Pass `--from-deposit` to send from your deterministic Polymarket deposit wallet via a relayer **`WALLET`** batch (same EIP-712 pattern as deposit-wallet approvals in-app). Also requires **`POLYMARKET_RELAYER_API_KEY`** and **`POLYMARKET_RELAYER_API_KEY_ADDRESS`**. Optional **`RELAYER_URL`** (defaults to production `https://relayer-v2.polymarket.com`). |

Examples:

```sh
# From repo root — transfer from EOA (omit --amount to send full token balance)
cd scripts && node transfer-erc20.mjs --token 0xYourToken --to 0xRecipient --amount 10 --decimals 6

# From deposit wallet (SIG_TYPE=3 style funding address)
cd scripts && node transfer-erc20.mjs --from-deposit --token 0xYourToken --to 0xRecipient --amount 10
```

Self-check for the CREATE2 deposit-wallet address (matches [`src/deposit_wallet.rs`](src/deposit_wallet.rs)):

```sh
cd scripts && node transfer-erc20.mjs --verify-derive
```

Full flags and behavior are documented in the script header.

## Known limitations

- **Trailing + resting limits.** If a limit buy that first went `live` later fills with you as **taker** on the user-channel trade, the app may not register that fill for trailing (maker-leg path only). Prefer a market buy or an immediately matched limit if you rely on the trail.
- **Trailing after restart.** With `BUY_TRAIL_BPS > 0` (stored at market roll), trade replay re-registers a trail from **long size + VWAP `avg_entry`**; the in-trail peak ratchet starts over from the new session’s book (no remembered pre-crash peak).
- **No full web-app parity.** Edge cases like partial fills or unusual order states may not match what the website shows.
- **No on-chain allowance setup.** Assumes spender approvals are already set. If not, run a one-time approval script (see [NautilusTrader docs](https://nautilustrader.io/docs/latest/integrations/polymarket/) for reference).
- **Relayer required for redemption.** The `x` key only works with `POLYMARKET_RELAYER_API_KEY` and `POLYMARKET_SIG_TYPE=2`. Plain EOA users need the web Portfolio page.
- **In-memory fill history.** PnL and fills reset on restart. Swap the `VecDeque<Fill>` for SQLite if you need persistence.
- **5m and 15m only in the wizard.** Daily markets are supported in the discovery code but not exposed in the UI.

## License

MIT — no warranty.
