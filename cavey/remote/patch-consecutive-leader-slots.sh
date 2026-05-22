#!/usr/bin/env bash
# Patch consecutive leader slots to 2 for cluster experiments.
# Requires patch-crates/solana-clock synced to AGAVE_DIR (see build-all.sh).

apply_consecutive_leader_slots_patch() {
  : "${AGAVE_DIR:?AGAVE_DIR required}"

  local repo_dir lib_rs cargo_toml
  repo_dir="$(expand_path "$AGAVE_DIR")"
  lib_rs="${repo_dir}/leader-schedule/src/lib.rs"
  cargo_toml="${repo_dir}/Cargo.toml"

  if [[ ! -f "$lib_rs" ]]; then
    echo "Missing ${lib_rs}" >&2
    return 1
  fi
  if [[ ! -d "${repo_dir}/patch-crates/solana-clock" ]]; then
    echo "Missing ${repo_dir}/patch-crates/solana-clock — sync patch-crates before build" >&2
    return 1
  fi

  if grep -q 'NonZeroUsize::new(2)' "$lib_rs" && \
     grep -q 'path = "patch-crates/solana-clock"' "$cargo_toml"; then
    echo "Consecutive leader slots already set to 2."
    return 0
  fi

  sed -i 's/NonZeroUsize::new(4)/NonZeroUsize::new(2)/' "$lib_rs"
  sed -i 's/NUM_CONSECUTIVE_LEADER_SLOTS: u64 = 4/NUM_CONSECUTIVE_LEADER_SLOTS: u64 = 2/' \
    "${repo_dir}/patch-crates/solana-clock/src/lib.rs"

  if ! grep -q 'path = "patch-crates/solana-clock"' "$cargo_toml"; then
    sed -i '/^\[patch.crates-io\]/a solana-clock = { path = "patch-crates/solana-clock" }' "$cargo_toml"
  fi

  touch "${repo_dir}/.cavey-force-rebuild"
  echo "Patched NUM_CONSECUTIVE_LEADER_SLOTS to 2 (leader-schedule + solana-clock)."
}
