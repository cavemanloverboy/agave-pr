#!/usr/bin/env bash
# Start faucet + bootstrap, then validators 2..N.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

init_run_logs start
log_banner

start_bootstrap() {
  local log
  log="$(node_log_file 1 "$BOOTSTRAP_IP")"
  local status=0

  run_remote_script_logged "$BOOTSTRAP_IP" "$here/remote/start-bootstrap.sh" "$log" \
    "AGAVE_DIR=${REMOTE_AGAVE_DIR}" \
    "BOOTSTRAP_IP=${BOOTSTRAP_IP}" || status=$?

  report_node 1 "$status" "$log"
  return "$status"
}

start_validator() {
  local node=$1
  local ip=$2
  local vlog
  vlog="$(node_log_file "$node" "$ip")"
  local vstatus=0

  run_remote_script_logged "$ip" "$here/remote/start-validator.sh" "$vlog" \
    "AGAVE_DIR=${REMOTE_AGAVE_DIR}" \
    "BOOTSTRAP_IP=${BOOTSTRAP_IP}" \
    "VALIDATOR_INDEX=${node}" \
    "NODE_IP=${ip}" || vstatus=$?

  report_node "$node" "$vstatus" "$vlog"
  return "$vstatus"
}

echo "starting bootstrap and validators together (wait-for-supermajority)..."
pids=()
start_bootstrap &
pids+=($!)

for ((i = 2; i <= NUM_VALIDATORS; i++)); do
  start_validator "$i" "$(node_ip "$i")" &
  pids+=($!)
done

if ! wait_parallel "${pids[@]}"; then
  echo "cluster start failed — see logs above" >&2
  exit 1
fi

rpc_ok=0
echo "waiting for bootstrap RPC on ${BOOTSTRAP_IP}:8899..."
for attempt in $(seq 1 60); do
  if ssh_cmd "$BOOTSTRAP_IP" \
    "export PATH=\$HOME/.cargo/bin:\$PATH; solana -u http://${BOOTSTRAP_IP}:8899 slot 2>/dev/null | grep -q ."; then
    rpc_ok=1
    echo "bootstrap RPC up (attempt ${attempt})"
    break
  fi
  if (( attempt % 6 == 0 )); then
    echo "  still waiting... ${attempt}/60 ($(((attempt * 5)))s) — tail: ./cavey/run.sh logs tail"
  fi
  sleep 5
done

if (( ! rpc_ok )); then
  echo "node 1 FAILED — bootstrap RPC did not come up" >&2
  exit 1
fi

echo "all nodes started"
echo "next: ./cavey/run.sh bench"
