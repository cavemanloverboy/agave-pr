#!/usr/bin/env bash
# Patch SIMD-0525 declare_id! pubkeys to match local feature authority keypairs.
# Inlined via run_remote_script; do not set -euo here (build.sh owns that).

apply_feature_key_patch() {
  : "${AGAVE_DIR:?AGAVE_DIR required}"
  : "${FEATURE_PUBKEY_350MS:?FEATURE_PUBKEY_350MS required}"
  : "${FEATURE_PUBKEY_300MS:?FEATURE_PUBKEY_300MS required}"
  : "${FEATURE_PUBKEY_250MS:?FEATURE_PUBKEY_250MS required}"
  : "${FEATURE_PUBKEY_200MS:?FEATURE_PUBKEY_200MS required}"

  local repo_dir lib_rs marker want
  repo_dir="$(expand_path "$AGAVE_DIR")"
  lib_rs="${repo_dir}/feature-set/src/lib.rs"

  if [[ ! -f "$lib_rs" ]]; then
    echo "Missing ${lib_rs} — is this the simd_0525 branch?" >&2
    return 1
  fi

  marker="${repo_dir}/.cavey-feature-keys"
  want="${FEATURE_PUBKEY_350MS}:${FEATURE_PUBKEY_300MS}:${FEATURE_PUBKEY_250MS}:${FEATURE_PUBKEY_200MS}"
  if [[ -f "$marker" ]] && [[ "$(cat "$marker")" == "$want" ]]; then
    echo "Feature key IDs already patched."
    return 0
  fi

  patch_id() {
    local old=$1
    local new=$2
    if grep -q "declare_id!(\"${new}\")" "$lib_rs"; then
      return 0
    fi
    if ! grep -q "declare_id!(\"${old}\")" "$lib_rs"; then
      echo "Expected feature id ${old} not found in ${lib_rs}" >&2
      return 1
    fi
    sed -i "s|declare_id!(\"${old}\")|declare_id!(\"${new}\")|g" "$lib_rs"
  }

  patch_id iBRL2iJvhLssJveF1utbmmQGmjonmNYZALcJFEHTbUF "$FEATURE_PUBKEY_350MS"
  patch_id iBRLA3zvd6x9445cK1vS7xt8n6Y7DS3otfRcDdW8JRW "$FEATURE_PUBKEY_300MS"
  patch_id iBRLR6nG3fDi8YD4mpPTUVTgo5NaiYfZgrzokCfADP2 "$FEATURE_PUBKEY_250MS"
  patch_id iBRLypKvvj9VEvwoTeRpbLhbW55NFR4T3GE9BUR8A16 "$FEATURE_PUBKEY_200MS"

  echo "$want" >"$marker"
  touch "${repo_dir}/.cavey-force-rebuild"
  echo "Patched SIMD-0525 feature IDs:"
  echo "  350ms -> ${FEATURE_PUBKEY_350MS}"
  echo "  300ms -> ${FEATURE_PUBKEY_300MS}"
  echo "  250ms -> ${FEATURE_PUBKEY_250MS}"
  echo "  200ms -> ${FEATURE_PUBKEY_200MS}"
}

if [[ "${BASH_SOURCE[0]:-}" == "${0}" ]]; then
  apply_feature_key_patch
fi
