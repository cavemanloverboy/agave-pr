#!/usr/bin/env bash
# Clone and release-build agave on every node in parallel (quiet; logs per node).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

init_run_logs build
log_banner

env_args=()
while IFS= read -r line; do
  [[ -n "$line" ]] && env_args+=("$line")
done < <(feature_pubkey_env)

build_one() {
  local node=$1
  local ip=$2
  local log
  log="$(node_log_file "$node" "$ip")"
  local status=0

  ssh_cmd "$ip" "mkdir -p $(printf '%q' "$REMOTE_AGAVE_DIR")/patch-crates"
  scp_cmd -r "$REPO_ROOT/patch-crates/solana-clock" \
    "${SSH_USER}@${ip}:${REMOTE_AGAVE_DIR}/patch-crates/"

  run_remote_script_logged "$ip" "$here/remote/build.sh" "$log" \
      "REPO_URL=${REPO_URL}" \
      "GIT_FETCH_REF=${GIT_FETCH_REF}" \
      "GIT_BRANCH=${GIT_BRANCH}" \
      "AGAVE_DIR=${REMOTE_AGAVE_DIR}" \
      "${env_args[@]}" || status=$?

  report_node "$node" "$status" "$log"
  return "$status"
}

pids=()
for i in "${!NODES[@]}"; do
  build_one "$((i + 1))" "${NODES[$i]}" &
  pids+=($!)
done

if ! wait_parallel "${pids[@]}"; then
  echo "build failed — see logs above" >&2
  exit 1
fi

echo "all nodes built"
