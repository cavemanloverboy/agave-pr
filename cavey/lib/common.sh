# shellcheck shell=bash
# Shared helpers for cavey bare-metal orchestration scripts.

set -euo pipefail

CAVEY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "$CAVEY_DIR/.." && pwd)"
CONFIG_FILE="$CAVEY_DIR/config.env"

if [[ ! -f "$CONFIG_FILE" ]]; then
  echo "Missing $CONFIG_FILE — copy cavey/config.env.example to cavey/config.env and edit it." >&2
  exit 1
fi

# shellcheck source=/dev/null
source "$CONFIG_FILE"

SSH_USER="${SSH_USER:?SSH_USER must be set in config.env}"
REMOTE_AGAVE_DIR="${REMOTE_AGAVE_DIR:-~/agave}"
SSH_KEY="${SSH_KEY:-}"
if [[ -n "$SSH_KEY" && ${SSH_KEY:0:1} == '~' ]]; then
  SSH_KEY="${SSH_KEY/#\~/$HOME}"
fi

if [[ ${#NODES[@]} -lt 1 ]]; then
  echo "NODES must list at least one IP in config.env" >&2
  exit 1
fi

BOOTSTRAP_IP="${NODES[0]}"
NUM_VALIDATORS="${#NODES[@]}"

# shellcheck source=lib/feature-keys.sh
source "$CAVEY_DIR/lib/feature-keys.sh"

SSH_OPTS=(
  -o BatchMode=yes
  -o ConnectTimeout=20
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
)
if [[ -n "$SSH_KEY" ]]; then
  SSH_OPTS+=(-i "$SSH_KEY")
fi

ssh_cmd() {
  local host=$1
  shift
  ssh "${SSH_OPTS[@]}" "${SSH_USER}@${host}" "$@"
}

scp_cmd() {
  scp "${SSH_OPTS[@]}" "$@"
}

remote_env() {
  cat <<EOF
export PATH="\$HOME/.cargo/bin:\$PATH"
[[ -f "\$HOME/.cargo/env" ]] && source "\$HOME/.cargo/env"
export USE_INSTALL=1
export CARGO_BUILD_PROFILE=release
export AGAVE_DIR="${REMOTE_AGAVE_DIR}"
EOF
}

run_remote_script() {
  local host=$1
  local script=$2
  shift 2
  local remote_env_args=()
  while [[ $# -gt 0 ]]; do
    remote_env_args+=("$1")
    shift
  done
  {
    remote_env
    printf '%s\n' "${remote_env_args[@]}"
    # Piped via `bash -s`: $0 is not the script path, so inline lib helpers.
    cat "$CAVEY_DIR/remote/patch-feature-keys.sh"
    cat "$CAVEY_DIR/remote/lib.sh"
    cat "$CAVEY_DIR/remote/patch-consecutive-leader-slots.sh"
    cat "$CAVEY_DIR/remote/storage-paths.sh"
    cat "$script"
  } | ssh_cmd "$host" "bash -s"
}

node_ip() {
  local index=$1
  if (( index < 1 || index > NUM_VALIDATORS )); then
    echo "Invalid validator index: $index (valid: 1..$NUM_VALIDATORS)" >&2
    return 1
  fi
  echo "${NODES[$((index - 1))]}"
}

require_bootstrap_reachable() {
  ssh_cmd "$BOOTSTRAP_IP" "echo ok" >/dev/null
}

# --- quiet parallel runs with per-node log files ---

init_run_logs() {
  local step=$1
  CAVEY_LOG_STEP=$step
  CAVEY_LOG_DIR="$CAVEY_DIR/logs/$(date +%Y%m%d-%H%M%S)-${step}"
  mkdir -p "$CAVEY_LOG_DIR"
  ln -sfn "$CAVEY_LOG_DIR" "$CAVEY_DIR/logs/latest"
}

log_banner() {
  printf 'Logs: %s\n' "$CAVEY_LOG_DIR"
}

node_log_file() {
  local node=$1
  local ip=$2
  echo "${CAVEY_LOG_DIR}/node-$(printf '%02d' "$node")-${ip}.log"
}

run_remote_script_logged() {
  local host=$1
  local script=$2
  local logfile=$3
  shift 3
  local remote_env_args=()
  while [[ $# -gt 0 ]]; do
    remote_env_args+=("$1")
    shift
  done
  {
    echo "=== $(date -Iseconds) ${CAVEY_LOG_STEP:-run} on ${host} ==="
  } >"$logfile"
  {
    remote_env
    printf '%s\n' "${remote_env_args[@]}"
    cat "$CAVEY_DIR/remote/patch-feature-keys.sh"
    cat "$CAVEY_DIR/remote/lib.sh"
    cat "$CAVEY_DIR/remote/patch-consecutive-leader-slots.sh"
    cat "$CAVEY_DIR/remote/storage-paths.sh"
    cat "$script"
  } | ssh_cmd "$host" "bash -s" >>"$logfile" 2>&1
}

report_node() {
  local node=$1
  local status=$2
  local logfile=$3
  if (( status == 0 )); then
    echo "node ${node} success"
  else
    echo "node ${node} FAILED — ${logfile}" >&2
  fi
}

wait_parallel() {
  local failed=0
  local pid
  for pid in "$@"; do
    wait "$pid" || failed=1
  done
  return "$failed"
}
