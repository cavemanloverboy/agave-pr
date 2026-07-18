#!/usr/bin/env bash
#
# Run a non-voting agave-validator on mainnet-beta.
#
# Usage:
#   ./scripts/run-validator-mainnet.sh

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

profile="${CARGO_BUILD_PROFILE:-release}"
export PATH="$root/target/$profile:$PATH"

stuff="${VALIDATOR_STUFF:-$root/validator-stuff-mainnet}"
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

mainnet_url="${MAINNET_URL:-https://api.mainnet-beta.solana.com}"
genesis_hash="$(solana genesis-hash --url "$mainnet_url")"

echo "Starting mainnet-beta validator (non-voting)."
echo "State directory: $stuff"

exec agave-validator \
  --ledger "$ledger" \
  --accounts "$accounts" \
  --snapshots "$snapshots" \
  --identity "$identity" \
  --no-voting \
  --no-snapshots \
  --only-known-rpc \
  --known-validator 7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2 \
  --known-validator GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ \
  --known-validator DE1bawNcRJB9rVm3buyMVfr8mBEoyyu73NBovf2oXJsJ \
  --known-validator CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S \
  --log - \
  --allow-private-addr \
  --no-port-check \
  --no-poh-speed-test \
  --limit-ledger-size \
  --wal-recovery-mode skip_any_corrupted_record \
  --expected-genesis-hash "$genesis_hash" \
  --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint2.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint3.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint4.mainnet-beta.solana.com:8001 \
  --entrypoint entrypoint5.mainnet-beta.solana.com:8001
