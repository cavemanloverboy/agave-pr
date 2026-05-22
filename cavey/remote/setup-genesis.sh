#!/usr/bin/env bash
set -euo pipefail

: "${AGAVE_DIR:?AGAVE_DIR required}"
: "${REPO_URL:?REPO_URL required}"
: "${GIT_FETCH_REF:?GIT_FETCH_REF required}"
: "${GIT_BRANCH:?GIT_BRANCH required}"
: "${NUM_VALIDATORS:?NUM_VALIDATORS required}"
: "${SLOTS_PER_EPOCH:?SLOTS_PER_EPOCH required}"
: "${CLUSTER_TYPE:?CLUSTER_TYPE required}"
: "${STAKE_LAMPORTS:?STAKE_LAMPORTS required}"

ensure_repo
ensure_build

cd "$(expand_path "$AGAVE_DIR")"
export PATH="$HOME/.cargo/bin:$PATH"
export USE_INSTALL=1
export CARGO_BUILD_PROFILE=release

KEY_DIR=config/keys
rm -rf "$KEY_DIR" config/bootstrap-validator
mkdir -p "$KEY_DIR"

echo "Generating keypairs for ${NUM_VALIDATORS} validators..."
for i in $(seq 1 "$NUM_VALIDATORS"); do
  solana-keygen new --no-passphrase -so "${KEY_DIR}/validator-identity-${i}.json"
  solana-keygen new --no-passphrase -so "${KEY_DIR}/validator-vote-${i}.json"
  solana-keygen new --no-passphrase -so "${KEY_DIR}/validator-stake-${i}.json"
done
solana-keygen new --no-passphrase -so "${KEY_DIR}/faucet.json"

export FAUCET_KEYPAIR="${KEY_DIR}/faucet.json"
export BOOTSTRAP_VALIDATOR_IDENTITY_KEYPAIR="${KEY_DIR}/validator-identity-1.json"
export BOOTSTRAP_VALIDATOR_VOTE_KEYPAIR="${KEY_DIR}/validator-vote-1.json"
export BOOTSTRAP_VALIDATOR_STAKE_KEYPAIR="${KEY_DIR}/validator-stake-1.json"

setup_args=(
  --cluster-type "$CLUSTER_TYPE"
  --slots-per-epoch "$SLOTS_PER_EPOCH"
  --bootstrap-validator-stake-lamports "$STAKE_LAMPORTS"
)

for i in $(seq 2 "$NUM_VALIDATORS"); do
  bls_pubkey=$(solana-keygen bls_pubkey "${KEY_DIR}/validator-identity-${i}.json")
  setup_args+=(
    --bootstrap-validator
      "${KEY_DIR}/validator-identity-${i}.json"
      "${KEY_DIR}/validator-vote-${i}.json"
      "${KEY_DIR}/validator-stake-${i}.json"
    --bootstrap-validator-bls-pubkey "$bls_pubkey"
  )
done

echo "Creating genesis..."
./multinode-demo/setup.sh "${setup_args[@]}"

run_ledger_tool -l config/bootstrap-validator shred-version \
  --max-genesis-archive-unpacked-size 1073741824 | tee config/shred-version

run_ledger_tool -l config/bootstrap-validator verify \
  --halt-at-slot 0 --print-bank-hash --output json \
  | jq -r '.hash' | tee config/bank-hash

run_ledger_tool -l config/bootstrap-validator genesis-hash | tee config/genesis-hash

echo "Packing genesis ledger for validators 2..${NUM_VALIDATORS}..."
tar czf config/ledger-bundle.tar.gz -C config/bootstrap-validator \
  --exclude=identity.json \
  --exclude=vote-account.json \
  .

echo "Shred version: $(cat config/shred-version)"
echo "Bank hash (slot 0): $(cat config/bank-hash)"
echo "Genesis hash: $(cat config/genesis-hash)"

prepare_agave_storage bootstrap bootstrap-validator

echo "Genesis setup complete."
