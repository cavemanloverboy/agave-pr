#!/usr/bin/env bash
set -euo pipefail

: "${AGAVE_DIR:?AGAVE_DIR required}"
: "${BOOTSTRAP_IP:?BOOTSTRAP_IP required}"
: "${BENCH_TPS_TX_COUNT:?BENCH_TPS_TX_COUNT required}"
: "${BENCH_TPS_DURATION:?BENCH_TPS_DURATION required}"

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

if pgrep -f 'multinode-demo/bench-tps.sh' >/dev/null 2>&1; then
  echo "bench-tps already running."
  exit 0
fi

echo "Starting bench-tps (tx_count=${BENCH_TPS_TX_COUNT}, duration=${BENCH_TPS_DURATION}s)..."

nohup ./multinode-demo/bench-tps.sh \
  --url "http://${BOOTSTRAP_IP}:8899" \
  --entrypoint "${BOOTSTRAP_IP}:8001" \
  --faucet "${BOOTSTRAP_IP}:9900" \
  --tx-count "$BENCH_TPS_TX_COUNT" \
  --duration "$BENCH_TPS_DURATION" \
  --client-node-id config/keys/validator-identity-1.json \
  > logs/bench-tps.log 2>&1 &

echo $! > logs/bench-tps.pid
echo "bench-tps started. Log: ${AGAVE_DIR}/logs/bench-tps.log"
