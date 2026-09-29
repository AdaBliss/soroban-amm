#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WASM_DIR="${WASM_DIR:-$ROOT_DIR/target/wasm32v1-none/release}"
# Network cap from CONFIG_SETTING_CONTRACT_MAX_SIZE_BYTES on testnet/mainnet.
# Unoptimized artifacts are measured here for a quick pre-optimization guard;
# the deploy pipeline optimizes with `stellar contract optimize` before upload,
# so the real enforcement is in CI's optimized-artifact check.
MAX_BYTES="${WASM_MAX_BYTES:-131072}"
FAIL_ON_LIMIT=false
OPTIMIZED=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --fail-on-limit) FAIL_ON_LIMIT=true ;;
    --optimized)     OPTIMIZED=true ;;
    *) echo "Usage: $0 [--fail-on-limit] [--optimized]" >&2; exit 2 ;;
  esac
  shift
done

if $OPTIMIZED; then
  WASM_DIR="${WASM_DIR_OPTIMIZED:-$ROOT_DIR/target/wasm32v1-none/release/optimized}"
fi

shopt -s nullglob
artifacts=("$WASM_DIR"/*.wasm)

printf 'Contract WASM size report\n'
if $OPTIMIZED; then
  printf '(optimized artifacts)\n'
fi
printf '%s\n' '-------------------------'

if [[ ${#artifacts[@]} -eq 0 ]]; then
  echo "No WASM artifacts found in $WASM_DIR" >&2
  exit 1
fi

status=0
for wasm in "${artifacts[@]}"; do
  size=$(wc -c < "$wasm")
  human=$(numfmt --to=iec-i --suffix=B --format='%.1f' "$size" 2>/dev/null || printf '%sB' "$size")
  relative="${wasm#"$ROOT_DIR"/}"
  if (( size > MAX_BYTES )); then
    printf '%s: %s (%s bytes) EXCEEDS LIMIT (%s bytes)\n' "$relative" "$human" "$size" "$MAX_BYTES"
    status=1
  else
    printf '%s: %s (%s bytes)\n' "$relative" "$human" "$size"
  fi
done

printf '%s\n' '-------------------------'
if $FAIL_ON_LIMIT && (( status != 0 )); then
  exit "$status"
fi
exit 0
