#!/usr/bin/env bash
# Start bench-tps on the bootstrap node.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

init_run_logs bench
log_banner

log="$(node_log_file 1 "$BOOTSTRAP_IP")"
status=0

run_remote_script_logged "$BOOTSTRAP_IP" "$here/remote/start-bench-tps.sh" "$log" \
  "AGAVE_DIR=${REMOTE_AGAVE_DIR}" \
  "BOOTSTRAP_IP=${BOOTSTRAP_IP}" \
  "BENCH_TPS_TX_COUNT=${BENCH_TPS_TX_COUNT}" \
  "BENCH_TPS_DURATION=${BENCH_TPS_DURATION}" || status=$?

report_node 1 "$status" "$log"
(( status == 0 )) || exit 1

echo "bench-tps running on bootstrap"
