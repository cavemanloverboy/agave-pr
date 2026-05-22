#!/usr/bin/env bash
# Activate one SIMD-0525 feature on the cluster.
#
# Usage:
#   ./cavey/activate-feature.sh 350
#   ./cavey/activate-feature.sh 300|250|200
#
# Feature authority keypairs (cavey/keys/feature-*ms.json) must match the
# declare_id! pubkeys patched into feature-set (see remote/patch-feature-keys.sh).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

step="${1:-}"
if [[ -z "$step" ]]; then
  echo "Usage: $0 {350|300|250|200}" >&2
  exit 1
fi

case "$step" in
350|300|250|200) ;;
*)
  echo "Unknown step: $step (use 350, 300, 250, or 200)" >&2
  exit 1
  ;;
esac

key_path="$(feature_key_path "$step")"
feature_id="$(feature_pubkey "$step")"

remote_key="/tmp/feature-${step}ms.json"
scp_cmd "$key_path" "${SSH_USER}@${BOOTSTRAP_IP}:${remote_key}"

echo "Activating ${step}ms feature (${feature_id})..."
ssh_cmd "$BOOTSTRAP_IP" bash <<EOF
set -e
export PATH="\$HOME/.cargo/bin:\$PATH"
cd $(printf '%q' "$REMOTE_AGAVE_DIR")
# Custom genesis — use "development" so the CLI skips public-cluster genesis hash checks.
solana -u http://${BOOTSTRAP_IP}:8899 feature activate ${remote_key} development \
  --fee-payer config/keys/faucet.json
echo
solana -u http://${BOOTSTRAP_IP}:8899 feature status | grep -E '${feature_id}|Slot time' || true
rm -f ${remote_key}
EOF

echo "Done. New slot time takes effect the epoch after activation."
