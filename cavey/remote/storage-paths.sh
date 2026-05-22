#!/usr/bin/env bash
# Resolve ledger / accounts / snapshots paths on NVMe when available.

# One mount point per physical NVMe disk (skips /boot*), sorted by device name.
list_nvme_disk_mounts() {
  local dev name mp seen=$'\n'
  for dev in /dev/nvme*n1; do
    [[ -b "$dev" ]] || continue
    name="${dev##*/}"
    mp=""
    while read -r part_mp; do
      [[ -z "$part_mp" || "$part_mp" == /boot* ]] && continue
      mp="$part_mp"
      [[ "$part_mp" == "/" ]] && break
    done < <(lsblk -rn -o MOUNTPOINT "/dev/$name" 2>/dev/null | grep -v '^$' || true)
    if [[ -n "$mp" && "$seen" != *$'\n'"$mp"$'\n'* ]]; then
      echo "$mp"
      seen+="$mp"$'\n'
    fi
  done
}

storage_root_on_mount() {
  local mount=$1
  local repo_dir=$2
  if [[ "$mount" == "/" ]]; then
    echo "${repo_dir}/storage"
  else
    echo "${mount%/}/agave"
  fi
}

# role: bootstrap | validator
resolve_agave_storage() {
  local role=$1
  local repo_dir ledger_root accounts_root snapshots_root

  repo_dir="$(expand_path "$AGAVE_DIR")"

  if [[ -n "${STORAGE_LEDGER:-}" ]]; then
    AGAVE_LEDGER="$(expand_path "$STORAGE_LEDGER")"
    AGAVE_ACCOUNTS="$(expand_path "${STORAGE_ACCOUNTS:-$AGAVE_LEDGER/../accounts/$role}")"
    AGAVE_SNAPSHOTS="$(expand_path "${STORAGE_SNAPSHOTS:-$AGAVE_LEDGER/../snapshots/$role}")"
  else
    local -a mounts=()
    mapfile -t mounts < <(list_nvme_disk_mounts)

    if ((${#mounts[@]} >= 3)); then
      ledger_root="$(storage_root_on_mount "${mounts[0]}" "$repo_dir")"
      accounts_root="$(storage_root_on_mount "${mounts[1]}" "$repo_dir")"
      snapshots_root="$(storage_root_on_mount "${mounts[2]}" "$repo_dir")"
      echo "Storage: 3 NVMe mounts -> ledger=${mounts[0]} accounts=${mounts[1]} snapshots=${mounts[2]}"
    elif ((${#mounts[@]} >= 1)); then
      ledger_root="$(storage_root_on_mount "${mounts[0]}" "$repo_dir")"
      accounts_root="$ledger_root"
      snapshots_root="$ledger_root"
      echo "Storage: NVMe mount ${mounts[0]} -> ledger, accounts, snapshots"
    else
      ledger_root="${repo_dir}/data"
      accounts_root="$ledger_root"
      snapshots_root="$ledger_root"
      echo "WARNING: no NVMe mount found; using ${ledger_root}" >&2
    fi

    AGAVE_LEDGER="${ledger_root}/ledger/${role}"
    AGAVE_ACCOUNTS="${accounts_root}/accounts/${role}"
    AGAVE_SNAPSHOTS="${snapshots_root}/snapshots/${role}"
  fi

  mkdir -p "$AGAVE_LEDGER" "$AGAVE_ACCOUNTS" "$AGAVE_SNAPSHOTS"
  echo "  ledger=${AGAVE_LEDGER}"
  echo "  accounts=${AGAVE_ACCOUNTS}"
  echo "  snapshots=${AGAVE_SNAPSHOTS}"
}

# Point config/<name> at the NVMe ledger directory (genesis/setup expect config paths).
link_config_ledger() {
  local repo_dir=$1
  local config_name=$2
  local config_path="${repo_dir}/config/${config_name}"

  repo_dir="$(expand_path "$repo_dir")"
  if [[ -L "$config_path" ]]; then
    rm -f "$config_path"
  elif [[ -d "$config_path" ]]; then
    if [[ -n "$(ls -A "$config_path" 2>/dev/null)" ]]; then
      echo "Migrating existing ${config_path} -> ${AGAVE_LEDGER}..."
      rsync -a "$config_path"/ "$AGAVE_LEDGER"/
    fi
    rm -rf "$config_path"
  fi
  mkdir -p "$(dirname "$config_path")"
  ln -sfn "$AGAVE_LEDGER" "$config_path"
}

prepare_agave_storage() {
  local role=$1
  local config_name=$2

  resolve_agave_storage "$role"
  link_config_ledger "$AGAVE_DIR" "$config_name"
}
