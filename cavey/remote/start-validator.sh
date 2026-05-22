#!/usr/bin/env bash
set -euo pipefail

: "${AGAVE_DIR:?AGAVE_DIR required}"
: "${BOOTSTRAP_IP:?BOOTSTRAP_IP required}"
: "${VALIDATOR_INDEX:?VALIDATOR_INDEX required}"
: "${NODE_IP:?NODE_IP required}"

ensure_ulimit

expand_path() {
  local path=$1
  path="${path/#\~/$HOME}"
  echo "$path"
}

AGAVE_DIR="$(expand_path "$AGAVE_DIR")"
cd "$AGAVE_DIR"

export PATH="$HOME/.cargo/bin:$PATH"
export USE_INSTALL=1

SHRED_VERSION="$(cat config/shred-version)"
BANK_HASH="$(cat config/bank-hash)"
GENESIS_HASH="$(cat config/genesis-hash)"
mkdir -p logs

prepare_agave_storage validator validator

if [[ ! -d "$AGAVE_LEDGER/rocksdb" ]]; then
  echo "Missing genesis ledger at ${AGAVE_LEDGER} — run ./cavey/run.sh keys first" >&2
  exit 1
fi

if pgrep -f "validator-${VALIDATOR_INDEX}.log" >/dev/null 2>&1; then
  echo "Validator ${VALIDATOR_INDEX} may already be running on $(hostname)."
fi

echo "Starting validator ${VALIDATOR_INDEX} on ${NODE_IP}..."

nohup prlimit --nofile=1000000:1000000 --memlock=unlimited:unlimited \
  agave-validator \
  --identity config/identity.json \
  --vote-account config/vote-account.json \
  --ledger "$AGAVE_LEDGER" \
  --accounts "$AGAVE_ACCOUNTS" \
  --snapshots "$AGAVE_SNAPSHOTS" \
  --bind-address "$NODE_IP" \
  --entrypoint "${BOOTSTRAP_IP}:8001" \
  --gossip-port 8001 \
  --expected-shred-version "$SHRED_VERSION" \
  --expected-bank-hash "$BANK_HASH" \
  --expected-genesis-hash "$GENESIS_HASH" \
  --wait-for-supermajority 0 \
  --no-genesis-fetch \
  --no-snapshot-fetch \
  --allow-private-addr \
  --full-rpc-api \
  --no-incremental-snapshots \
  --max-genesis-archive-unpacked-size 1073741824 \
  --no-poh-speed-test \
  --no-os-network-limits-test \
  --require-tower \
  --log "logs/validator-${VALIDATOR_INDEX}.log" \
  > "logs/validator-${VALIDATOR_INDEX}.out" 2>&1 &

echo $! > "logs/validator-${VALIDATOR_INDEX}.pid"
sleep 2
if ! kill -0 "$(cat "logs/validator-${VALIDATOR_INDEX}.pid")" 2>/dev/null; then
  echo "Validator ${VALIDATOR_INDEX} failed to start:" >&2
  tail -30 "logs/validator-${VALIDATOR_INDEX}.out" >&2 || true
  exit 1
fi
echo "Validator ${VALIDATOR_INDEX} started."
