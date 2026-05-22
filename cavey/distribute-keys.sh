#!/usr/bin/env bash
# Copy validator keys, genesis metadata, and ledger from bootstrap to validators 2..N.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib/common.sh
source "$here/lib/common.sh"

require_bootstrap_reachable

init_run_logs keys
log_banner

bundle_log="$(node_log_file 1 "$BOOTSTRAP_IP")"
{
  ssh_cmd "$BOOTSTRAP_IP" bash <<EOF
set -euo pipefail
cd $(printf '%q' "$REMOTE_AGAVE_DIR")
test -f config/bank-hash
test -f config/shred-version
if [[ ! -f config/ledger-bundle.tar.gz ]]; then
  if [[ ! -f config/genesis-hash ]]; then
    export PATH="\$HOME/.cargo/bin:\$PATH"
    agave-ledger-tool -l config/bootstrap-validator genesis-hash | tee config/genesis-hash
  fi
  echo "Packing genesis ledger..."
  tar czf config/ledger-bundle.tar.gz -C config/bootstrap-validator \
    --exclude=identity.json \
    --exclude=vote-account.json \
    .
fi
ls -lh config/ledger-bundle.tar.gz
EOF
} >>"$bundle_log" 2>&1 || {
  echo "failed to prepare ledger bundle on bootstrap — $bundle_log" >&2
  exit 1
}

distribute_one() {
  local node=$1
  local ip=$2
  local log
  log="$(node_log_file "$node" "$ip")"
  local status=0

  {
    ssh_cmd "$ip" "mkdir -p $(printf '%q' "$REMOTE_AGAVE_DIR")/config"
    scp_cmd \
      "${SSH_USER}@${BOOTSTRAP_IP}:${REMOTE_AGAVE_DIR}/config/keys/validator-identity-${node}.json" \
      "${SSH_USER}@${ip}:${REMOTE_AGAVE_DIR}/config/identity.json"
    scp_cmd \
      "${SSH_USER}@${BOOTSTRAP_IP}:${REMOTE_AGAVE_DIR}/config/keys/validator-vote-${node}.json" \
      "${SSH_USER}@${ip}:${REMOTE_AGAVE_DIR}/config/vote-account.json"
    scp_cmd \
      "${SSH_USER}@${BOOTSTRAP_IP}:${REMOTE_AGAVE_DIR}/config/keys/validator-stake-${node}.json" \
      "${SSH_USER}@${ip}:${REMOTE_AGAVE_DIR}/config/stake-account.json"
    scp_cmd \
      "${SSH_USER}@${BOOTSTRAP_IP}:${REMOTE_AGAVE_DIR}/config/shred-version" \
      "${SSH_USER}@${ip}:${REMOTE_AGAVE_DIR}/config/shred-version"
    scp_cmd \
      "${SSH_USER}@${BOOTSTRAP_IP}:${REMOTE_AGAVE_DIR}/config/bank-hash" \
      "${SSH_USER}@${ip}:${REMOTE_AGAVE_DIR}/config/bank-hash"
    scp_cmd \
      "${SSH_USER}@${BOOTSTRAP_IP}:${REMOTE_AGAVE_DIR}/config/genesis-hash" \
      "${SSH_USER}@${ip}:${REMOTE_AGAVE_DIR}/config/genesis-hash"
    scp_cmd \
      "${SSH_USER}@${BOOTSTRAP_IP}:${REMOTE_AGAVE_DIR}/config/ledger-bundle.tar.gz" \
      "${SSH_USER}@${ip}:${REMOTE_AGAVE_DIR}/config/ledger-bundle.tar.gz"
    {
      remote_env
      printf '%s\n' "AGAVE_DIR=${REMOTE_AGAVE_DIR}"
      cat "$CAVEY_DIR/remote/patch-feature-keys.sh"
      cat "$CAVEY_DIR/remote/lib.sh"
      cat "$CAVEY_DIR/remote/storage-paths.sh"
      cat "$here/remote/extract-validator-ledger.sh"
    } | ssh_cmd "$ip" "bash -s"
  } >"$log" 2>&1 || status=$?

  report_node "$node" "$status" "$log"
  return "$status"
}

pids=()
for ((i = 2; i <= NUM_VALIDATORS; i++)); do
  distribute_one "$i" "$(node_ip "$i")" &
  pids+=($!)
done

if ! wait_parallel "${pids[@]}"; then
  echo "key distribution failed — see logs above" >&2
  exit 1
fi

echo "all keys and genesis ledger distributed"
echo "next: ./cavey/run.sh start"
