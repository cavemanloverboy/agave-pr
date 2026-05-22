#!/usr/bin/env bash
# Query SIMD-0525 slot-time features on bootstrap (must use cluster solana, not local CLI).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

pubkeys=()
for step in 350 300 250 200; do
  pubkeys+=("$(feature_pubkey "$step")")
done

ssh_cmd "$BOOTSTRAP_IP" bash <<EOF
set -e
export PATH="\$HOME/.cargo/bin:\$PATH"
solana -u http://${BOOTSTRAP_IP}:8899 feature status ${pubkeys[*]}
EOF
