#!/usr/bin/env bash
# Stop faucet, validators, and bench-tps on all nodes.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

init_run_logs stop
log_banner

stop_one() {
  local node=$1
  local ip=$2
  local log
  log="$(node_log_file "$node" "$ip")"
  local status=0

  {
    ssh_cmd "$ip" bash <<'EOF'
pkill -f 'multinode-demo/bench-tps.sh' 2>/dev/null || true
pkill -f 'multinode-demo/bootstrap-validator.sh' 2>/dev/null || true
pkill -f 'multinode-demo/validator.sh' 2>/dev/null || true
pkill -f 'multinode-demo/faucet.sh' 2>/dev/null || true
pkill -f 'agave-validator' 2>/dev/null || true
pkill -f 'solana-faucet' 2>/dev/null || true
pkill -f 'solana-bench-tps' 2>/dev/null || true
EOF
  } >"$log" 2>&1 || status=$?

  report_node "$node" "$status" "$log"
  return "$status"
}

pids=()
for i in "${!NODES[@]}"; do
  stop_one "$((i + 1))" "${NODES[$i]}" &
  pids+=($!)
done

wait_parallel "${pids[@]}" || true
echo "stop sent to all nodes"
