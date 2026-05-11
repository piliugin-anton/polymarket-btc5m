#!/bin/sh
set -e
# Profile token: btc-5m, eth-15m, sol-5m, xrp-1d, etc. (see polymarket-crypto run --help)
exec /usr/local/bin/polymarket-crypto run --market "${MARKET:-sol-5m}"
