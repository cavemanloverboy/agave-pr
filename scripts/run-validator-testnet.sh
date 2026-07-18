#!/usr/bin/env bash
#
# Run a non-voting agave-validator on testnet with local state under ./validator-stuff.
#
# Usage:
#   ./scripts/run-validator-testnet.sh

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

profile="${CARGO_BUILD_PROFILE:-release}"
export PATH="$root/target/$profile:$PATH"

stuff="$root/validator-stuff"
ledger="$stuff/ledger"
accounts="$stuff/accounts"
snapshots="$stuff/snapshots"
identity="$stuff/identity.json"

mkdir -p "$ledger" "$accounts" "$snapshots"

if [[ ! -f "$identity" ]]; then
  echo "creating identity keypair at $identity"
  solana-keygen new --no-passphrase -so "$identity"
fi

echo "building agave-validator ($profile) ..."
cargo build --profile "$profile" --bin agave-validator

testnet_url="${TESTNET_URL:-https://api.testnet.solana.com}"
genesis_hash="$(solana genesis-hash --url "$testnet_url")"

exec agave-validator \
  --ledger "$ledger" \
  --accounts "$accounts" \
  --snapshots "$snapshots" \
  --identity "$identity" \
  --no-voting \
  --full-rpc-api \
  --rpc-port "${RPC_PORT:-8899}" \
  --log - \
  --allow-private-addr \
  --no-port-check \
  --no-poh-speed-test \
  --expected-genesis-hash "$genesis_hash" \
  --entrypoint entrypoint.testnet.solana.com:8001 \
  --entrypoint entrypoint2.testnet.solana.com:8001 \
  --entrypoint entrypoint3.testnet.solana.com:8001
