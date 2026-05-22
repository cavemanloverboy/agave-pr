#!/usr/bin/env bash
set -euo pipefail

: "${REPO_URL:?REPO_URL required}"
: "${GIT_FETCH_REF:?GIT_FETCH_REF required}"
: "${GIT_BRANCH:?GIT_BRANCH required}"
: "${AGAVE_DIR:?AGAVE_DIR required}"

ensure_repo

echo "Building commit $(git rev-parse --short HEAD) on $(hostname)..."
ensure_build

echo "Installed binaries:"
ls -1 "$HOME/.cargo/bin"/agave-validator "$HOME/.cargo/bin"/solana 2>/dev/null || true
