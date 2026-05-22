#!/usr/bin/env bash
set -euo pipefail

: "${AGAVE_DIR:?AGAVE_DIR required}"

cd "$(expand_path "$AGAVE_DIR")"
prepare_agave_storage validator validator
rm -rf "$AGAVE_LEDGER"
mkdir -p "$AGAVE_LEDGER"
tar xzf config/ledger-bundle.tar.gz -C "$AGAVE_LEDGER"
