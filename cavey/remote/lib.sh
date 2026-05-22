#!/usr/bin/env bash
# Shared helpers for scripts that run on remote nodes.

expand_path() {
  local path=$1
  path="${path/#\~/$HOME}"
  echo "$path"
}

ensure_repo() {
  : "${REPO_URL:?REPO_URL required}"
  : "${GIT_FETCH_REF:?GIT_FETCH_REF required}"
  : "${GIT_BRANCH:?GIT_BRANCH required}"
  : "${AGAVE_DIR:?AGAVE_DIR required}"

  local repo_dir
  repo_dir="$(expand_path "$AGAVE_DIR")"

  if [[ ! -d "$repo_dir/.git" ]]; then
    echo "Cloning ${REPO_URL} -> ${repo_dir}..."
    git clone "$REPO_URL" "$repo_dir"
  fi

  cd "$repo_dir"
  git fetch origin "${GIT_FETCH_REF}"
  git checkout -B "${GIT_BRANCH}" FETCH_HEAD
  echo "Checked out $(git rev-parse --short HEAD) on $(hostname)"

  if [[ -n "${FEATURE_PUBKEY_350MS:-}" ]]; then
    apply_feature_key_patch
  fi

  if [[ -d "${repo_dir}/patch-crates/solana-clock" ]]; then
    apply_consecutive_leader_slots_patch
  fi
}

ensure_ulimit() {
  local desired_nofile=1000000
  local current
  current=$(ulimit -n)

  if [[ ! -f /etc/security/limits.d/99-agave-nofile.conf ]]; then
    echo "Configuring open file limit to ${desired_nofile} (sudo)..."
    sudo tee /etc/security/limits.d/99-agave-nofile.conf >/dev/null <<EOF
* soft nofile ${desired_nofile}
* hard nofile ${desired_nofile}
EOF
    echo "fs.file-max = $((desired_nofile * 2))" | sudo tee /etc/sysctl.d/99-agave-nofile.conf >/dev/null
    sudo sysctl --system >/dev/null 2>&1 || sudo sysctl -w "fs.file-max=$((desired_nofile * 2))" >/dev/null
  fi

  if [[ ! -f /etc/security/limits.d/99-agave-memlock.conf ]]; then
    echo "Configuring memlock limit for Agave io_uring (sudo)..."
    sudo tee /etc/security/limits.d/99-agave-memlock.conf >/dev/null <<'EOF'
* soft memlock unlimited
* hard memlock unlimited
EOF
  fi

  ulimit -n "$desired_nofile" 2>/dev/null || \
    sudo prlimit --nofile="${desired_nofile}:${desired_nofile}" --pid=$$ >/dev/null 2>&1 || true
  ulimit -l unlimited 2>/dev/null || \
    sudo prlimit --memlock=unlimited:unlimited --pid=$$ >/dev/null 2>&1 || true

  current=$(ulimit -n)
  if (( current < desired_nofile )); then
    echo "Error: nofile is ${current} (need ${desired_nofile}). Log out/in or reboot after limits install." >&2
    return 1
  fi
}

run_ledger_tool() {
  ensure_ulimit
  prlimit --nofile=1000000:1000000 --memlock=unlimited:unlimited \
    agave-ledger-tool "$@"
}

ensure_prerequisites() {
  if [[ ! -f "$HOME/.cavey-agave-prereqs" ]]; then
    echo "Installing system build dependencies (sudo)..."
    sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
      build-essential curl git pkg-config \
      libssl-dev libudev-dev zlib1g-dev \
      llvm clang cmake make \
      libprotobuf-dev protobuf-compiler libclang-dev
    touch "$HOME/.cavey-agave-prereqs"
  fi

  ensure_ulimit

  if ! command -v rustup >/dev/null || ! command -v cargo >/dev/null; then
    echo "Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  fi

  # shellcheck source=/dev/null
  [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
  export PATH="$HOME/.cargo/bin:$PATH"

  if command -v rustup >/dev/null; then
    rustup component add rustfmt 2>/dev/null || true
  fi
}

ensure_build() {
  ensure_prerequisites
  cd "$(expand_path "$AGAVE_DIR")"

  export PATH="$HOME/.cargo/bin:$PATH"
  local force_rebuild=0
  if [[ -f .cavey-force-rebuild ]]; then
    force_rebuild=1
  fi
  if (( ! force_rebuild )) && command -v solana-keygen >/dev/null && command -v agave-validator >/dev/null && \
     command -v agave-ledger-tool >/dev/null && command -v solana >/dev/null; then
    echo "Binaries already installed."
    return 0
  fi

  rm -f .cavey-force-rebuild
  echo "Building release binaries..."
  ./fetch-perf-libs.sh
  export CARGO_BUILD_PROFILE=release
  scripts/cargo-install-all.sh "$HOME/.cargo" \
    --no-build-dev-bins \
    --no-build-deprecated-bins \
    --no-build-platform-tools \
    --no-spl-token
}
