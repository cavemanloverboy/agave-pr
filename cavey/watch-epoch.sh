#!/usr/bin/env bash
# Poll epoch info, validators skip rate, and feature status from bootstrap.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

INTERVAL="${1:-2}"

while true; do
  clear
  date
  echo "Bootstrap: ${BOOTSTRAP_IP}"
  echo

  ssh_cmd "$BOOTSTRAP_IP" bash <<EOF
set -e
export PATH="\$HOME/.cargo/bin:\$PATH"
RPC=http://${BOOTSTRAP_IP}:8899

echo "=== epoch-info ==="
solana -u \$RPC epoch-info 2>/dev/null || echo "(RPC not ready)"

echo
echo "=== validators (skip rate) ==="
solana -u \$RPC validators 2>/dev/null | head -20 || echo "(no validator data)"

echo
echo "=== SIMD-0525 slot time (350/300/250/200 ms) ==="
solana -u \$RPC feature status \
  $(for step in 350 300 250 200; do feature_pubkey "$step"; done | tr '\n' ' ') \
  2>/dev/null || echo "(feature status failed — run ./cavey/feature-status.sh)"
EOF

  echo
  echo "Refreshing in ${INTERVAL}s (Ctrl-C to stop)..."
  sleep "$INTERVAL"
done
