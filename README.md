# Polymarket "Bitcoin Up or Down 5m" trading terminal

A Rust TUI for trading Polymarket's 5-minute **Bitcoin Up or Down** prediction
markets. Colors the live Chainlink BTC/USD price green/red against each
window's opening "Price to Beat", shows both sides of the order book, your
current UP/DOWN positions with unrealized PnL, and lets you fire **FAK**
market orders or **GTD** limit orders that auto-expire just before the
current 5m window closes — all from single-key actions.

## Design at a glance

```
┌─ BTC 5m Predictions ──────────────────────────────────────────────────┐
│ BTC/USD (Chainlink)  $  67,432.51   ▲   +32.51                        │
│ Price to Beat: $67,400.00   Closes in 02:34   Bitcoin Up or Down…     │
├───────────────────────────────────────────┬───────────────────────────┤
│ Order Book                                │ Positions                 │
│  UP price    size  │ DOWN price    size   │   UP                      │
│   0.62   ×   200   │  0.38   ×  200       │   120.00 sh @ 0.580       │
│   0.61   ×   150   │  0.39   ×  150       │          mark 0.620       │
│   ── 0.605 ──      │  ── 0.395 ──         │          uPnL $+4.80      │
│   0.60   ×   500   │  0.40   ×  500       │   DOWN                    │
│                                           │   Realized   $+12.00      │
│                                           │   Total      $+16.80      │
├───────────────────────────────────────────┴───────────────────────────┤
│ Fills                                                                 │
│ 20:23:15 BUY  UP   120.00  @ 0.580  = $69.60                          │
│ 20:18:42 SELL UP   100.00  @ 0.650  = $65.00  +7.00                   │
├───────────────────────────────────────────────────────────────────────┤
│ size [5.00] USDC   [u] buy UP  [d] buy DOWN  [U] sell UP  [D] sell …  │
│ › BUY 2.00 UP @ 0.620 ✓                                               │
└───────────────────────────────────────────────────────────────────────┘
```

## Architecture

```
┌────────────────────┐   ┌──────────────────────┐   ┌───────────────────┐
│  Crossterm keys    │──▶│                      │◀──│ Chainlink RTDS WS │
└────────────────────┘   │   mpsc<AppEvent>     │   └───────────────────┘
┌────────────────────┐   │       ↓              │   ┌───────────────────┐
│  Gamma poll (10s)  │──▶│    AppState          │◀──│ CLOB book WS      │
│  + market roll     │   │       ↓              │   │ (restart on roll) │
└────────────────────┘   │  ratatui::draw       │   └───────────────────┘
                         └──────────────────────┘
                                ↓ actions
                         ┌──────────────────────┐
                         │  TradingClient       │
                         │  alloy EIP-712 sign  │
                         │  reqwest → CLOB REST │
                         └──────────────────────┘
```

Four async tasks push into a single `mpsc<AppEvent>` channel. The main loop
drains events, mutates `AppState`, and redraws. Key events run through
`events::handle_key`, which returns a pure `Action` that the runtime then
dispatches on a worker task — no I/O happens on the render loop.

**Signing.** Orders are built against the on-chain `Order` struct from
[`ctf-exchange`](https://github.com/Polymarket/ctf-exchange) and signed with
EIP-712 using `alloy` + `alloy-sol-types`. L1 auth (`ClobAuthDomain` struct)
derives the L2 API credentials on first run; every subsequent REST call is
authed with HMAC-SHA256 over `ts + method + path + body`.

**Market discovery.** The Gamma API's `/events` endpoint is polled every 10s
with the `crypto-5m` tag filter. The bot picks the `btc-updown-5m-*` event
whose `[start_date, end_date)` contains the current UTC time and auto-rolls
when it closes.

**Price feed.** `wss://ws-live-data.polymarket.com` → topic
`crypto_prices_chainlink` → filter `symbol=btc/usd`. This is the exact
feed Polymarket resolves against, so the header color is consistent with
what decides your position.

## Debugging

Every run writes a log to `./btc5m-bot.log` (override with `BTC5M_LOG_PATH`).
Default log level is `debug` — every HTTP response body, every WS subscribe
message, every EIP-712 digest is captured there. `tail -f btc5m-bot.log` in
another pane while the TUI runs.

If CLOB auth fails, the TUI's status line will scroll through the error
chain over ~10 seconds. For the definitive dump, quit the TUI and run:

```sh
./target/release/btc5m-bot debug-auth
```

That prints everything: the signer address, the funder, the proxy status,
the EIP-712 type hash, domain separator, struct hash, final digest, the
signature, then hits both `/auth/derive-api-key` and `/auth/api-key` and
shows the exact status + body you got back. If the digest looks right but
the server still says 401, the problem is usually one of:

- **Signer address doesn't match POLY_ADDRESS** — check that your
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

Polymarket blocks a broad set of IPs at the edge. If `cargo run` shows no BTC
price, no order book, and Gamma errors in the log, you're almost certainly
being blocked. Set `POLYMARKET_PROXY` in `.env` and everything — REST
(Gamma, CLOB REST) plus WebSockets (Chainlink RTDS, CLOB book) — will
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
./target/release/btc5m-bot debug-auth
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
cd btc5m-bot
cp .env.example .env
# ...edit .env with your keys
cargo build --release
```

### Run

```sh
RUST_LOG=btc5m_bot=debug ./target/release/btc5m-bot
```

## Key bindings

Normal mode:

| key     | action                                 |
|---------|----------------------------------------|
| `u` / `d` | **market BUY** UP / DOWN (FAK at best ask) |
| `U` / `D` | **market SELL** UP / DOWN (FAK at best bid) |
| `l`     | open limit-order modal                 |
| `c`     | cancel ALL open orders                 |
| `s`     | edit persistent ticket size            |
| `r`     | force-refresh active market            |
| `q` / `Esc` / `Ctrl-C` | quit                      |

Limit modal:

| key     | action                                 |
|---------|----------------------------------------|
| ← / →   | flip outcome (UP ↔ DOWN)               |
| ↑ / ↓   | flip side (BUY ↔ SELL)                 |
| `Tab`   | switch price / size field              |
| digits / `.` | edit current field                |
| `Enter` | submit as **GTD** limit order |
| `Esc`   | cancel modal                           |

GTD expiration is chosen so the order stops resting about **one second before** the active market’s `closes_at`. The CLOB expects a unix `expiration` field with Polymarket’s **+60s** security buffer on top of that instant (see [Create order → GTD](https://docs.polymarket.com/developers/CLOB/orders/create-order)). **CLOB API signing version must be 1** (EIP-712 includes `expiration`); if `/version` returns `2`, GTD placement is rejected until the client supports it.

Size edit mode:

| key     | action                                 |
|---------|----------------------------------------|
| digits / `.` | edit size buffer                  |
| `Enter` / `Esc` | commit (reverts to default on parse fail) |

## What's intentionally *not* here

- **User channel WebSocket (authenticated fills).** Fills today are
  synthesised from successful order acks. For a tighter PnL, subscribe to
  `wss://ws-subscriptions-clob.polymarket.com/ws/user` with L2 auth and
  merge into `AppState` — same code path as book events.
- **On-chain allowance setting.** Assumed pre-approved; if not, run a
  one-time script to call `USDC.approve` and `CTF.setApprovalForAll` for the
  two Exchange contracts.
- **Winnings redemption.** When a market resolves, your winning shares sit
  in the Safe until you redeem via `CTF.redeemPositions`. Out of scope for
  this bot — the web UI handles it with one click.
- **Persistence.** Realized PnL resets on restart. Swap the `VecDeque<Fill>`
  for a sqlite table if you want history across sessions.

## Project layout

```
src/
├── main.rs            # tokio runtime, terminal init, action dispatch
├── config.rs          # env vars, endpoints, SignatureType enum
├── app.rs             # AppState, Position, Fill, event reducer
├── events.rs          # keyboard → Action (pure, unit-testable)
├── gamma.rs           # Gamma REST client, ActiveMarket
├── trading.rs         # EIP-712 Order sign + L1/L2 auth + CLOB POST
├── feeds/
│   ├── chainlink.rs   # RTDS WS → PriceTick
│   └── clob_ws.rs     # CLOB market WS → BookSnapshot
└── ui/
    └── render.rs      # full ratatui render in one function
```

## Safety

This bot signs orders on your behalf. Until you're confident in its
behaviour:

1. Start with `DEFAULT_SIZE_USDC=1.0` or `0.5`. Orders below ~$1 on
   Polymarket are often rejected by the CLOB minimum-order-size check —
   useful dry-run signal.
2. Tune `MARKET_BUY_SLIPPAGE_BPS` / `MARKET_SELL_SLIPPAGE_BPS` (use `0` for no
   cushion); legacy `MARKET_SLIPPAGE_BPS` still sets either side if unset.
3. Never check your private key into source control. The `.env.example`
   file ships with zeros specifically so that `cp .env.example .env` fails
   loudly if you forget to edit.

## License

MIT — do what you want, no warranty.