#!/usr/bin/env bash
set -euo pipefail

: "${AGAVE_DIR:?AGAVE_DIR required}"
: "${BOOTSTRAP_IP:?BOOTSTRAP_IP required}"

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

mkdir -p logs

prepare_agave_storage bootstrap bootstrap-validator

if pgrep -f 'multinode-demo/bootstrap-validator.sh' >/dev/null 2>&1 || \
   pgrep -f 'agave-validator' >/dev/null 2>&1; then
  echo "Stopping existing bootstrap validator..."
  pkill -f 'multinode-demo/bootstrap-validator.sh' 2>/dev/null || true
  pkill -f 'agave-validator' 2>/dev/null || true
  sleep 2
fi

if pgrep -f 'multinode-demo/faucet.sh' >/dev/null 2>&1; then
  echo "Faucet already running."
else
  echo "Starting faucet..."
  nohup ./multinode-demo/faucet.sh > logs/faucet.log 2>&1 &
  echo $! > logs/faucet.pid
fi

sleep 2

if pgrep -f 'agave-validator' >/dev/null 2>&1; then
  echo "Bootstrap validator already running."
else
  BANK_HASH="$(cat config/bank-hash)"
  SHRED_VERSION="$(cat config/shred-version)"
  echo "Starting bootstrap validator on ${BOOTSTRAP_IP}..."
  nohup prlimit --nofile=1000000:1000000 --memlock=unlimited:unlimited \
    agave-validator \
    --identity config/bootstrap-validator/identity.json \
    --vote-account config/bootstrap-validator/vote-account.json \
    --ledger "$AGAVE_LEDGER" \
    --accounts "$AGAVE_ACCOUNTS" \
    --snapshots "$AGAVE_SNAPSHOTS" \
    --bind-address "$BOOTSTRAP_IP" \
    --gossip-port 8001 \
    --rpc-port 8899 \
    --rpc-faucet-address 127.0.0.1:9900 \
    --snapshot-interval-slots 200 \
    --wait-for-supermajority 0 \
    --expected-bank-hash "$BANK_HASH" \
    --expected-shred-version "$SHRED_VERSION" \
    --require-tower \
    --no-incremental-snapshots \
    --no-poh-speed-test \
    --no-os-network-limits-test \
    --no-wait-for-vote-to-start-leader \
    --full-rpc-api \
    --allow-private-addr \
    --log logs/validator.log \
    > logs/bootstrap.out 2>&1 &
  echo $! > logs/bootstrap.pid
fi

echo "Bootstrap started. Logs: ${AGAVE_DIR}/logs/"
