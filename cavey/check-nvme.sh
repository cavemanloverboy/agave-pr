#!/usr/bin/env bash
# Report NVMe layout and resolved Agave storage paths on all cluster nodes.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

check_one() {
  local node=$1
  local ip=$2

  echo "=== node ${node} (${ip}) ==="
  run_remote_script "$ip" "$here/remote/check-nvme-remote.sh" \
    "AGAVE_DIR=${REMOTE_AGAVE_DIR}" || true
  echo
}

for i in "${!NODES[@]}"; do
  check_one "$((i + 1))" "${NODES[$i]}"
done
