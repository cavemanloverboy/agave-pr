#!/usr/bin/env bash
set -euo pipefail

: "${AGAVE_DIR:?AGAVE_DIR required}"

echo "--- block devices ---"
lsblk -d -o NAME,SIZE,MODEL,TRAN,MOUNTPOINTS 2>/dev/null | grep -E 'NAME|nvme' || lsblk -d

echo "--- NVMe mount points ---"
mapfile -t mounts < <(list_nvme_disk_mounts)
if ((${#mounts[@]} == 0)); then
  echo "(none)"
else
  printf '  %s\n' "${mounts[@]}"
fi

echo "--- bootstrap storage ---"
prepare_agave_storage bootstrap bootstrap-validator

echo "--- validator storage ---"
prepare_agave_storage validator validator
