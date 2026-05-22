#!/usr/bin/env bash
# Create genesis on the bootstrap node with N staked validators.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

init_run_logs genesis
log_banner

log="$(node_log_file 1 "$BOOTSTRAP_IP")"
status=0

env_args=()
while IFS= read -r line; do
  [[ -n "$line" ]] && env_args+=("$line")
done < <(feature_pubkey_env)

run_remote_script_logged "$BOOTSTRAP_IP" "$here/remote/setup-genesis.sh" "$log" \
  "AGAVE_DIR=${REMOTE_AGAVE_DIR}" \
  "REPO_URL=${REPO_URL}" \
  "GIT_FETCH_REF=${GIT_FETCH_REF}" \
  "GIT_BRANCH=${GIT_BRANCH}" \
  "NUM_VALIDATORS=${NUM_VALIDATORS}" \
  "SLOTS_PER_EPOCH=${SLOTS_PER_EPOCH}" \
  "CLUSTER_TYPE=${CLUSTER_TYPE}" \
  "STAKE_LAMPORTS=${STAKE_LAMPORTS:-100000000000}" \
  "BOOTSTRAP_IP=${BOOTSTRAP_IP}" \
  "${env_args[@]}" || status=$?

report_node 1 "$status" "$log"
(( status == 0 )) || exit 1

echo "next: ./cavey/run.sh keys"
