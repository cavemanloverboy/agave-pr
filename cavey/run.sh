#!/usr/bin/env bash
# End-to-end bare-metal SIMD-0525 cluster bring-up (run steps manually or all at once).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

usage() {
  cat <<EOF
Usage: $0 <command>

Commands:
  build       Clone + release build on all nodes (parallel, quiet)
  genesis     Create genesis on bootstrap (quiet)
  keys        Distribute validator keys bootstrap -> nodes 2..N (parallel)
  start       Start faucet, bootstrap, and validators (parallel)
  bench       Start bench-tps on bootstrap
  watch       Poll epoch / skip rate / SIMD-0525 features
  features    Show 350/300/250/200 ms feature status (via bootstrap solana)
  nvme        Show NVMe layout and storage paths on all nodes
  stop        Stop all processes on all nodes (parallel)
  logs        Show latest log directory / tail a node log
  all         build -> genesis -> keys -> start -> bench

Setup:
  cp cavey/config.env.example cavey/config.env
  # edit IPs and SSH_USER

Logs:
  Per-run logs under cavey/logs/<timestamp>-<step>/
  Symlink: cavey/logs/latest/

EOF
}

cmd="${1:-}"
case "$cmd" in
build)
  "$here/build-all.sh"
  ;;
genesis)
  "$here/setup-genesis.sh"
  ;;
keys)
  "$here/distribute-keys.sh"
  ;;
start)
  "$here/start-all.sh"
  ;;
bench)
  "$here/start-bench-tps.sh"
  ;;
watch)
  exec "$here/watch-epoch.sh" "${2:-2}"
  ;;
features)
  "$here/feature-status.sh"
  ;;
nvme)
  "$here/check-nvme.sh"
  ;;
stop)
  "$here/stop-all.sh"
  ;;
logs)
  shift || true
  # shellcheck source=lib/common.sh
  source "$here/lib/common.sh"
  latest="$CAVEY_DIR/logs/latest"
  if [[ ! -d "$latest" ]]; then
    echo "No logs yet. Run a cavey command first." >&2
    exit 1
  fi
  case "${1:-}" in
  list)
    ls -1 "$latest"
    ;;
  tail)
    node="${2:-1}"
    # shellcheck source=lib/common.sh
    ip="$(node_ip "$node")"
    file=$(ls "$latest"/node-$(printf '%02d' "$node")-"${ip}".log 2>/dev/null | head -1)
    if [[ -z "$file" ]]; then
      file=$(ls "$latest"/node-$(printf '%02d' "$node")-*.log 2>/dev/null | head -1)
    fi
    if [[ -z "$file" ]]; then
      echo "No log for node ${node} in ${latest}" >&2
      exit 1
    fi
    tail -f "$file"
    ;;
  *)
    echo "$latest"
    ls -1 "$latest" 2>/dev/null | sed 's/^/  /'
    ;;
  esac
  ;;
all)
  "$here/build-all.sh"
  "$here/setup-genesis.sh"
  "$here/distribute-keys.sh"
  "$here/start-all.sh"
  "$here/start-bench-tps.sh"
  echo
  echo "Cluster is up. Monitor with: ./cavey/run.sh watch"
  echo "Activate features at epoch boundaries:"
  echo "  ./cavey/activate-feature.sh 350"
  echo "  ./cavey/activate-feature.sh 300"
  echo "  ./cavey/activate-feature.sh 250"
  echo "  ./cavey/activate-feature.sh 200"
  ;;
*)
  usage
  exit 1
  ;;
esac
