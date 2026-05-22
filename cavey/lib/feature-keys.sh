# shellcheck shell=bash
# Resolve SIMD-0525 feature authority pubkeys from config.env key files.

feature_key_path() {
  local step=$1
  local var="FEATURE_KEY_${step}MS"
  local path="${!var:-}"
  if [[ -z "$path" ]]; then
    echo "FEATURE_KEY_${step}MS not set in config.env" >&2
    return 1
  fi
  if [[ ${path:0:1} == '~' ]]; then
    path="${path/#\~/$HOME}"
  fi
  if [[ ! -f "$path" ]]; then
    echo "Feature key not found: $path" >&2
    return 1
  fi
  echo "$path"
}

feature_pubkey() {
  local step=$1
  local key_path
  key_path="$(feature_key_path "$step")"
  if ! command -v solana-keygen >/dev/null; then
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
  solana-keygen pubkey "$key_path"
}

feature_pubkey_env() {
  local step pubkey
  for step in 350 300 250 200; do
    pubkey="$(feature_pubkey "$step")"
    echo "FEATURE_PUBKEY_${step}MS=${pubkey}"
  done
}
