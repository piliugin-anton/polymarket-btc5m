# Polymarket crypto up/down trading terminal

A Rust TUI for Polymarket’s rolling **Up or Down** crypto prediction markets
(**BTC, ETH, SOL, XRP**), with **5-minute** and **15-minute** windows. On launch, a
short **wizard** loads live series from Gamma: pick an asset, then a timeframe,
then the terminal shows the same layout as before — live Chainlink **spot** price
(via RTDS) vs each window’s opening **Price to Beat**, full UP/DOWN order book,
your positions with unrealized PnL, optional **Sentiment** (CLOB mid or Data API
top holders), **FAK** market orders and **GTD** limit orders that expire just
before the window closes, plus **Polymarket Bridge** Solana USDC deposit (**`f`**) —
all from single-key actions where possible.

## Architecture

```
┌─────────────────────┐   ┌───────────────────────────────┐   ┌────────────────────┐
│ Crossterm keys      │   │                               │   │ Chainlink RTDS WS  │
│ + resize / focus    │──▶│  mpsc<AppEvent> (bounded)     │◀──│ per-asset spot     │
└─────────────────────┘   │  coalesce bursts (price/book) │   │  (e.g. btc/usd)    │
┌─────────────────────┐   │            ↓                  │   └────────────────────┘
│ 1 Hz Tick           │──▶│         AppState              │   ┌─────────────────────┐
└─────────────────────┘   │            ↓                  │   │ CLOB market WS      │
┌─────────────────────┐   │     ratatui::draw             │   │ CLOB user WS        │
│ Gamma poll 15s/1s   │──▶│  throttled ~20 Hz on feeds    │   │ (fills, subs roll)  │
│ (asset + 5m/15m)    │   │                               │   └─────────────────────┘
│                     │   └───────────────┬───────────────┘
└─────────────────────┘                   │
         ┌────────────────────────────────┼────────────────────────────┐
         ▼                                ▼                            ▼
┌─────────────────┐              ┌──────────────────┐        ┌─────────────────────┐
│ Gamma REST      │              │ TradingClient    │        │ Data API (HTTP)     │
│ ActiveMarket    │              │ EIP-712 orders   │        │ positions, holders, │
│ resolution      │              │ L1/L2 CLOB REST  │        │ neg-risk claimable  │
└─────────────────┘              └──────────────────┘        │ (roll bootstrap)    │
                                                             └──────────┬──────────┘
                                                                        │
                                                             ┌──────────▼──────────┐
                                                             │ Polygon JSON-RPC    │
                                                             │ (Multicall3         │
                                                             │  `aggregate3`)      │
                                                             │  panel cash + CTF)  │
                                                             └─────────────────────┘
```

Many async producers share one `mpsc<AppEvent>` channel (buffer 512): keyboard
and focus/resize handling, a 1&nbsp;Hz ticker, Chainlink price (via a small
forwarder that keeps only the latest tick per burst), CLOB book snapshots (via
a forwarder that merges concurrent UP/DOWN updates), **CLOB user-channel** trades
and order hints (long-lived WSS, market subscription swaps on each roll), market
rolls, position / open-order / balance / **top-holders** snapshots, and order
status lines. The main loop drains
events in batches, applies them to `AppState`, and calls `Terminal::draw`. When
a batch has **no** key events, redraws are **throttled** (`FEED_REDRAW_MIN`,
50&nbsp;ms) so feed-heavy sessions do not pin a CPU core — see ratatui
discussion around high-frequency `draw`.

Before the trading screen, a **startup wizard** (`gamma_series` + Gamma) lists
assets with live 5m series metadata; the user picks **5m** or **15m** and
`AppEvent::StartTrading` spawns RTDS, discovery, and feed tasks for that
[`MarketProfile`](src/market_profile.rs).

Key events go through `events::handle_key`, which returns a pure `Action`. The
runtime dispatches trading, cancel, **redeem all** (`x` / `X`), and **bridge
deposit** (`f`) on separate
`tokio` tasks — **no** network I/O on the render path. On startup, API
credential derivation also runs in the background so the TUI can paint before
L2 auth completes.

**Signing.** Orders use the on-chain `Order` shape from
[`ctf-exchange`](https://github.com/Polymarket/ctf-exchange), signed with
EIP-712 (`alloy` + `alloy-sol-types`). L1 auth derives L2 credentials once;
later REST calls use HMAC-SHA256 over `ts + method + path + body`.

**Networking.** `net` builds a proxy-aware `reqwest` client and WebSocket
tunnels (`POLYMARKET_PROXY`: HTTP `CONNECT` or SOCKS5, then TLS + WS) shared by
Gamma, CLOB REST, Data API, RTDS, CLOB **market** socket, and CLOB **user**
socket. The **balance panel** uses a **separate** HTTP client to `POLYGON_RPC_URL`
only (no proxy) so `eth_call` reads stay fast and are not routed through a
Polymarket-blocked path. **Polymarket Bridge** (`bridge.polymarket.com`) and
**public** Data API `GET /holders` use the same proxy-capable client as the rest
of the Polymarket-facing stack.

**Market discovery.** After you finish the wizard, a background task uses
`GammaClient::find_current_updown` for your [`MarketProfile`](src/market_profile.rs)
(rolling slugs like `{asset}-updown-{5m|15m}-<window_start>`). While the
current window is open it polls at most **every 15&nbsp;s** (and wakes at
`closes_at` so the next tick runs as soon as the window ends). **After** `closes_at`
it hits the **next** window slug at **1&nbsp;Hz** until Gamma returns 200. Only one
Gamma request runs at a time (async mutex). A supervisor aborts
the previous per-market **book** WebSocket and starts a new one on each roll; it
also updates the **user** WebSocket subscription, kicks off a positions sync (CLOB
balances + `/data/trades` replay, Data API sizes for escrowed sells), a 5&nbsp;s
open-order poller, and a top-holders poller for header **Sentiment**.

**Balances and claimable.** A 5&nbsp;s task reads **on-chain** values via Polygon
[Multicall3](https://github.com/mds1/multicall) **`aggregate3`** (one `eth_call`
per chunk): **USDC.e** cash (`balanceOf` on the bridged collateral token) and
**standard CTF** claimable from `payoutDenominator` / `payoutNumerators` +
ERC-1155 balances on the Conditional Tokens contract (see
[`balances.rs`](src/balances.rs)). The Data API lists **redeemable** markets
(standard) and supplies **neg-risk** claimable sums where position IDs differ.
**Redeem:** with `POLYMARKET_RELAYER_API_KEY` (+ `POLYMARKET_RELAYER_API_KEY_ADDRESS`)
set and `POLYMARKET_SIG_TYPE=2` (Gnosis Safe funder), **`x`** / **`X`** fetches
all redeemable rows from the Data API and submits **one** gasless Safe
`execTransaction` to the Polymarket relayer. Multiple distinct markets are
batched with Gnosis **`MultiSend`** + Safe **delegateCall** (same pattern as
[`@polymarket/builder-relayer-client`](https://github.com/Polymarket/builder-relayer-client));
a single market still uses a direct `redeemPositions` call on the CTF contract
or neg-risk adapter. Rows that cannot be built (bad ids, zero on-chain neg-risk
balances, etc.) are skipped with a log warning so the rest still redeem.

**Fees and take-profit.** `fees` implements Polymarket **crypto** taker fees for
PnL and for limit prices after optional **market BUY → GTD take-profit** sells
(`MARKET_BUY_TAKE_PROFIT_BPS`).

**Price feed.** `wss://ws-live-data.polymarket.com` → topic
`crypto_prices_chainlink` → `symbol=<asset>/usd` from the wizard (e.g. `btc/usd`,
`eth/usd`) — the same class of feed used for resolution, so the header aligns with
settlement. Polymarket’s HTTP **crypto-price** endpoint supplies the **Price to
Beat** (with description / RTDS latch as fallbacks; see `gamma` + `data_api`).

## Debugging

Every run writes a log to `./polymarket-btc5m.log` (override with `BTC5M_LOG_PATH`).
Default log level is `debug` — every HTTP response body, every WS subscribe
message, every EIP-712 digest is captured there. `tail -f polymarket-btc5m.log` in
another pane while the TUI runs.

If CLOB auth fails, the TUI's status line will scroll through the error
chain over ~10 seconds. For the definitive dump, quit the TUI and run:

```sh
./target/release/polymarket-btc5m debug-auth
```

That prints everything: the signer address, the funder, the proxy status,
the EIP-712 type hash, domain separator, struct hash, final digest, the
signature, then hits both `/auth/derive-api-key` and `/auth/api-key` and
shows the exact status + body you got back. If the digest looks right but
the server still says 401, the problem is usually one of:

- **Signer address doesn't match the wallet Polymarket expects** — check that your
  `POLYMARKET_PK` is for the right EOA. The `debug-auth` output shows the
  address derived from your key.
- **Clock skew** — `date -u` vs a known reference. Off by more than ~10s
  and the server rejects.
- **Wallet never set up on Polymarket** — log into polymarket.com with
  this EOA at least once to deploy the Safe; `create_or_derive_api_key`
  can't synthesise creds for a wallet the platform has never seen.
- **Proxy stripping headers** — some corporate/SOCKS proxies rewrite the
  `POLY_*` custom headers. Try a different proxy or a direct connection
  from a permitted region.

## Geo-restricted? Use a proxy

Polymarket blocks a broad set of IPs at the edge. If `cargo run` shows no
**spot** price, no order book, and Gamma errors in the log, you're almost
certainly being blocked. Set `POLYMARKET_PROXY` in `.env` and everything — REST
(Gamma, CLOB REST) plus WebSockets (Chainlink RTDS, CLOB book and user) — will
tunnel through it:

```
POLYMARKET_PROXY=http://user:pass@proxy.example.com:8080   # HTTP(S)
POLYMARKET_PROXY=socks5://127.0.0.1:1080                   # SOCKS5
```

For HTTP proxies the bot sends a `CONNECT` for each WebSocket before doing
TLS + the WS handshake. For SOCKS5 it uses `tokio-socks` to do the
handshake and then treats the tunnel as a plain TCP stream. TLS (via
`rustls` with `webpki-roots`) happens at the origin so your proxy only sees
ciphertext. A residential or datacenter proxy in any non-blocked region
works — Polymarket only inspects the peer IP, not headers.

You'll see `proxy=…` in the startup log line when it's active.

## Troubleshooting CLOB credentials

If you see `could not derive CLOB API credentials` in the startup log, run:

```sh
./target/release/polymarket-btc5m debug-auth
```

This skips the TUI, runs the L1 auth flow with verbose output, and prints
every intermediate value (typeHash, domainSeparator, structHash, digest,
signature) alongside the actual HTTP status and body returned by
`GET /auth/derive-api-key` and `POST /auth/api-key`. Common patterns:

| what `debug-auth` shows | diagnosis |
|---|---|
| `401 Unauthorized` on both, body mentions *signature* or *address* | `ecrecover(digest, sig)` returned a different address from `POLY_ADDRESS`. Either the typeHash is wrong (shouldn't be after the v0.1 fix), or `POLYMARKET_PK` doesn't match the account you think it does. |
| `401 Unauthorized`, body mentions *timestamp* | clock skew > ~10 s. Run `sudo ntpdate pool.ntp.org` (or `w32tm /resync` on Windows). |
| `403 Forbidden` on create | proxy is in a region Polymarket still blocks, or the wallet tripped their compliance layer. Try a different proxy region. |
| `404 Not Found` on derive, `200 OK` on create | your wallet had no prior API keys; now it does. The TUI will work on next launch. |
| `200 OK` on derive but TUI still errors | the creds are fine — the next failure is probably L2 (HMAC), likely a base64 decoding mismatch on the secret. |
| `warning: POLYMARKET_FUNDER equals your signer…` | misconfigured `sig_type`. EOA wallets need `sig_type=0` and `funder=eoa`. Safe users need `sig_type=2` and `funder=safe_address`. |

**Wallet never used on polymarket.com**: if this EOA/Safe has never placed a
trade through the web UI, the backend may have no record of it and will
reject API-key creation with a 403. Log in once at polymarket.com with the
same wallet, deposit $1 USDC, then re-run `debug-auth`.

**Comparing against py-clob-client**: the gold standard for verification is
to point the Python client at the same wallet with the same timestamp and
nonce, then compare the resulting `POLY_SIGNATURE` byte-for-byte against
what `debug-auth` prints. If they differ, the issue is in our signing; if
they match and Python works but Rust doesn't, the issue is in our HTTP
layer (headers, proxy, TLS).

## Setup

### Prerequisites

- Rust **1.80+** (`rustup toolchain install stable`)
- A funded Polymarket wallet. For the typical UX that means an EOA that owns
  a Gnosis Safe holding your USDC. Both addresses go in `.env`.
- The Safe must have already approved the CTF Exchange + NegRisk Exchange as
  spenders — if you've ever placed a trade through the web UI, this is
  already done. If not, see the
  [NautilusTrader setup script](https://nautilustrader.io/docs/latest/integrations/polymarket/)
  for reference allowance-setting code.

### Install

```sh
git clone <your-fork>
cd <repo-directory>
cp .env.example .env
# ...edit .env with your keys (incl. POLYGON_RPC_URL for on-chain balance reads)
cargo build --release
```

Set **`POLYGON_RPC_URL`** to a reliable Polygon HTTPS endpoint (Alchemy, drpc,
public `polygon-rpc.com`, etc.). The TUI balance panel does **not** use
`POLYMARKET_PROXY` for this URL. If you see empty Cash/Claimable, verify the URL
and that the process was restarted after editing `.env`.

### Run

```sh
RUST_LOG=polymarket-btc5m=debug ./target/release/polymarket-btc5m
```

On first launch the TUI **loads series** for each supported asset, then:

1. **Pick asset** (↑/↓ or `j`/`k`, Enter) — BTC, ETH, SOL, or XRP.
2. **Pick timeframe** — **5m** or **15m** (↑/↓ or `j`/`k`, Enter). `B` / `Esc` goes back;
   `q` / `Ctrl-C` quits.

After that you are in the trading screen. From there, **`Esc`** returns to the
timeframe step (not quit); use **`q`** or **Ctrl-C** to exit the app.

## Key bindings

Trading screen — normal mode:

| key     | action                                 |
|---------|----------------------------------------|
| `w` / `s` | **market BUY** UP / DOWN (FAK at best ask) — WASD layout |
| `a` / `d` | **market SELL** UP / DOWN (FAK at best bid) |
| `l`     | open limit-order modal                 |
| `c`     | cancel ALL open orders                 |
| `x` / `X` | **redeem all** claimable resolved positions (relayer + Safe; see *Balances and claimable*) |
| `e`     | edit persistent ticket size            |
| `f`     | **Polymarket Bridge** — fetch Solana (`svm`) USDC deposit address + terminal QR |
| `r`     | force-refresh active market            |
| `q` / `Ctrl-C` | quit                    |
| `Esc`   | leave trading → **timeframe** wizard step |

Holding a key down does **not** fire repeated market orders (`Repeat` is ignored).

**Sizing (Polymarket CLOB).** Per [Create order](https://docs.polymarket.com/developers/CLOB/orders/create-order), **FAK/FOK market BUY** is a **USDC dollar budget** (“specify the dollar amount you want to spend”); **market SELL** is **outcome shares**. **GTC/GTD limit BUY** uses **`size` in shares** at your limit price (this TUI’s limit modal still types BUY size as USDC notional, then converts to shares before submit). The `minimum_order_size` / `min_order_size` fields on [`getOrderBook`](https://docs.polymarket.com/developers/CLOB/clients/methods-public#getOrderBook) are **share** thresholds— they align with **limit** flow and **share-sized** legs, **not** a direct “\$5 minimum spend” on **market BUY**. Small **market BUY** tickets (e.g. **\$1** while the ask is **> 0.5**, i.e. fewer than five shares if fully filled at that price) can still match in practice; third-party guides often cite **~\$1** as a **market** floor and **~5 shares** for **limits** (e.g. [Start Polymarket — How to Trade](https://startpolymarket.com/guides/how-to-trade/)). If placement fails with `INVALID_ORDER_MIN_SIZE`, increase size or re-check book metadata for that token.

Limit modal:

| key     | action                                 |
|---------|----------------------------------------|
| `w` / `s` / `a` / `d` | same as trading screen: set outcome + side (UP/DOWN buy/sell) |
| ← / →   | flip outcome (UP ↔ DOWN)               |
| ↑ / ↓   | flip side (BUY ↔ SELL)                 |
| `Tab`   | switch price / size field              |
| digits / `.` | edit current field                |
| `Enter` | submit as **GTD** limit order          |
| `x` / `X` | **redeem all** (closes modal)      |
| `Esc`   | cancel modal                           |

The modal enforces at least **5 outcome shares** on submit after converting BUY notional → shares (SELL: size is already shares). That matches typical **`minimum_order_size`** on these rolling crypto up/down books and avoids `INVALID_ORDER_MIN_SIZE` on **GTD**; see `min_order_size` / `minimum_order_size` in Polymarket’s [order book](https://docs.polymarket.com/developers/CLOB/clients/methods-public#getOrderBook) / [market](https://docs.polymarket.com/developers/CLOB/clients/methods-public#getMarket) payloads (values can differ by slug).

GTD expiration is chosen so the order stops resting about **one second before** the active market’s `closes_at`. The CLOB expects a unix `expiration` field with Polymarket’s **+60s** security buffer on top of that instant (see [Create order → GTD](https://docs.polymarket.com/developers/CLOB/orders/create-order)). **CLOB API signing version must be 1** (EIP-712 includes `expiration`); if `/version` returns `2`, GTD placement is rejected until the client supports it.

Size edit mode:

| key     | action                                 |
|---------|----------------------------------------|
| digits / `.` | edit size buffer                  |
| `Enter` / `Esc` | commit (reverts to default on parse fail) |
| `x` / `X` | same as normal mode: **redeem all** (exits size edit first) |

## What's intentionally *not* here

- **Full parity with the web app.** The CLOB **user** WebSocket is connected
  (`wss://ws-subscriptions-clob.polymarket.com/ws/user`); trades and some order
  metadata merge into `AppState`, with REST + order-ack fallbacks. Edge cases
  (partial fills, unusual order states) may still differ from the website.
- **On-chain allowance setting.** Assumed pre-approved; if not, run a
  one-time script to call `USDC.approve` and `CTF.setApprovalForAll` for the
  two Exchange contracts.
- **Winnings redemption without relayer keys.** **`x`** batches redeemable
  markets through the relayer only when `POLYMARKET_RELAYER_API_KEY` and
  `POLYMARKET_RELAYER_API_KEY_ADDRESS` are set (Safe / `sig_type=2`). Otherwise
  use the web Portfolio **Claim** flow or another tool.
- **Persistence.** Realized PnL and fill history are in-memory. Swap the
  `VecDeque<Fill>` for a sqlite table if you want history across sessions.
- **Daily (1D) markets in the wizard.** Code supports **daily** calendar slugs
  (`Timeframe::D1`) in discovery; the first-run UI currently offers **5m** and
  **15m** only.

## Project layout

```
src/
├── main.rs                 # tokio runtime, TUI init, event loop, action dispatch, wizard
├── config.rs               # env vars, endpoints, SignatureType, CTF exchange addrs
├── app.rs                  # AppState, positions, fills, AppEvent reducer
├── events.rs               # keyboard → Action (pure)
├── market_profile.rs       # CryptoAsset list, Timeframe, rolling/daily slug helpers
├── gamma_series.rs         # Gamma `GET /series` for wizard rows
├── gamma.rs                # Gamma REST, ActiveMarket, find_current_updown, GTD exp
├── trading.rs              # EIP-712 orders, L1/L2 auth, CLOB REST, user-channel parse
├── balances.rs             # On-chain USDC.e + CTF claimable (Polygon Multicall3)
├── data_api.rs             # Data API: positions, holders, redeemable, neg-risk
├── bridge_deposit.rs       # Polymarket Bridge HTTP → Solana USDC deposit + QR
├── redeem.rs               # CTF redeem via Polymarket relayer (Safe)
├── fees.rs                 # crypto taker fee + take-profit limit price
├── net.rs                  # proxy-aware HTTP + WebSocket connect
├── feeds/
│   ├── chainlink.rs        # RTDS WS → PriceTick (symbol from profile)
│   ├── clob_ws.rs          # per-market CLOB WS → BookSnapshot
│   ├── clob_user_ws.rs     # authenticated CLOB user WS → fills, order hints
│   ├── user_trade_sync.rs  # trade replay / de-dupe vs user stream
│   └── market_discovery_gamma.rs  # Gamma poll + roll → ActiveMarket
└── ui/
    └── render.rs           # ratatui layout
```

## Safety

This bot signs orders on your behalf. Until you're confident in its
behaviour:

1. Start with a small `DEFAULT_SIZE_USDC`. **`w` / `s` market BUY** uses that
   value as a **USDC spend** budget (Polymarket **FAK BUY** = dollars; see
   [Create order → Order types](https://docs.polymarket.com/developers/CLOB/orders/create-order)),
   so **\$1** tickets can fill even when the implied share count is **below**
   book **`minimum_order_size`** at the current ask. **GTD limits** are blocked
   in-app below **5 shares** after notional→share conversion; you can still see
   `INVALID_ORDER_MIN_SIZE` or liquidity errors from the API for edge sizes—
   bump the ticket or check the book if that happens.
2. Tune `MARKET_BUY_SLIPPAGE_BPS` / `MARKET_SELL_SLIPPAGE_BPS` (use `0` for no
   cushion); legacy `MARKET_SLIPPAGE_BPS` still sets either side if unset.
3. Never check your private key into source control. The `.env.example`
   file ships with zeros specifically so that `cp .env.example .env` fails
   loudly if you forget to edit.

## License

MIT — do what you want, no warranty.