# polymarket-crypto

A terminal trading interface for Polymarket's rolling **Up or Down** crypto prediction markets (BTC, ETH, SOL, XRP), built in Rust with a full TUI.

---

> **Educational project — use at your own risk.**
> This software is developed for learning purposes. It interacts with real money on a live prediction market. There is no warranty of any kind. You are solely responsible for any funds lost due to bugs, misconfiguration, or unexpected market behavior. Start with very small amounts and verify every action before trusting it with real funds.

---

## What it does

Polymarket runs short-window crypto prediction markets: every 5 or 15 minutes a new "Up or Down?" market opens for BTC, ETH, SOL, or XRP. You bet on whether the price will be higher or lower than the opening price when the window closes.

This terminal app lets you:

- Watch the **live spot price** (Chainlink) vs each window's **Price to Beat**
- See the full **UP / DOWN order book** in real time
- Place **market orders** (FAK — Fill or Kill) and **limit orders** (GTD — expire just before window close) with single keypresses
- Track your **open positions and unrealized PnL**
- Use an optional **trailing stop** — tracks the best bid, sells automatically when the trail trips
- View **sentiment** (CLOB mid-price or top holders from Data API)
- **Redeem** resolved winnings via the Polymarket relayer (Safe wallets)
- Fetch a **Solana USDC deposit address** via Polymarket Bridge (key `f`)

Everything happens in the terminal. No browser required once you have your wallet configured.

## Quick start

### Prerequisites

- **Rust 1.80+** — install via [rustup](https://rustup.rs)
- A **funded Polymarket wallet**. The typical setup is an EOA (private key) that controls a Gnosis Safe holding your USDC. Both addresses go in `.env`.
- The Safe must have approved the CTF Exchange and NegRisk Exchange as spenders. If you've ever placed a trade through the Polymarket web UI, this is already done.
- A **Polygon RPC endpoint** (Alchemy, drpc, or the free `polygon-rpc.com`) for on-chain balance reads.

### Install

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
| `POLYMARKET_SIG_TYPE` | `2` for Gnosis Safe (default), `0` for plain EOA |
| `POLYGON_RPC_URL` | Polygon HTTPS endpoint for balance reads |
| `DEFAULT_SIZE_USDC` | Default order size in USDC (e.g. `5.0`) |

Optional but useful:

| Variable | What it is |
|---|---|
| `POLYMARKET_PROXY` | Proxy for geo-blocked regions — see [Geo-restrictions](#geo-restricted) |
| `MARKET_BUY_TRAIL_BPS` | Enable trailing stop (bps width from peak). `0` = off |
| `MARKET_BUY_TAKE_PROFIT_BPS` | Auto GTD limit sell after market buy (ignored when trailing is on) |
| `MARKET_BUY_SLIPPAGE_BPS` | Slippage cushion for market buys. Default `50` (0.5%) |
| `POLYMARKET_RELAYER_API_KEY` | Required for `x` / `X` redeem via Polymarket relayer (Safe only) |

See [`.env.example`](.env.example) for the full list and descriptions.

### Run

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
| `l` | Open limit-order modal |
| `c` | Cancel ALL open orders |
| `x` / `X` | Redeem all resolved positions (requires relayer key) |
| `e` | Edit default ticket size |
| `f` | Fetch Solana USDC deposit address + QR code |
| `r` | Force-refresh active market |
| `Esc` | Return to timeframe picker |
| `q` / Ctrl-C | Quit |

Holding a key down does **not** fire repeated orders — key repeat is ignored.

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

## Safety tips

1. Start with a small `DEFAULT_SIZE_USDC` (e.g. `1.0` or `5.0`) until you trust the setup.
2. Run `debug-auth` before your first trade to verify credentials are correct (see [Troubleshooting](#troubleshooting-clob-credentials)).
3. If you enable `MARKET_BUY_TRAIL_BPS`, understand that a trip fires a real FAK SELL — test with minimum size first.
4. Never commit `.env` with real keys. The example ships with zero-filled keys so a copy without editing will fail loudly.

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
./target/release/polymarket-crypto debug-auth
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

## Known limitations

- **No full web-app parity.** Edge cases like partial fills or unusual order states may not match what the website shows.
- **No on-chain allowance setup.** Assumes spender approvals are already set. If not, run a one-time approval script (see [NautilusTrader docs](https://nautilustrader.io/docs/latest/integrations/polymarket/) for reference).
- **Relayer required for redemption.** The `x` key only works with `POLYMARKET_RELAYER_API_KEY` and `POLYMARKET_SIG_TYPE=2`. Plain EOA users need the web Portfolio page.
- **In-memory fill history.** PnL and fills reset on restart. Swap the `VecDeque<Fill>` for SQLite if you need persistence.
- **5m and 15m only in the wizard.** Daily markets are supported in the discovery code but not exposed in the UI.

## License

MIT — no warranty.
