#!/usr/bin/env bash
# SSH to a cluster node using the same options as ./cavey/run.sh (no host key prompts).
#
# Usage:
#   ./cavey/ssh.sh 213.239.141.29
#   ./cavey/ssh.sh 213.239.141.29 'tail -f ~/agave/logs/validator.log'
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

ip="${1:-}"
shift || true

if [[ -z "$ip" ]]; then
  echo "Cluster nodes:"
  for i in "${!NODES[@]}"; do
    printf "  [%d] %s\n" "$((i + 1))" "${NODES[$i]}"
  done
  echo
  echo "Usage: $0 <ip> [remote command...]"
  exit 1
fi

if [[ $# -eq 0 ]]; then
  exec ssh "${SSH_OPTS[@]}" "${SSH_USER}@${ip}"
fi

ssh_cmd "$ip" "$@"
