#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/abi_contracts.sh"

WASM_DIR="${ABI_WASM_DIR:-$ROOT_DIR/target/wasm32v1-none/release}"
OUTPUT_FILE="${1:-${ABI_OUTPUT_FILE:-$ROOT_DIR/docs/abi.json}}"
STELLAR_BIN="${STELLAR_BIN:-stellar}"

if ! command -v "$STELLAR_BIN" >/dev/null 2>&1; then
    echo "ERROR: Stellar CLI '$STELLAR_BIN' was not found. Install the pinned version from .stellar-version." >&2
    exit 1
fi

if python3 -c 'import sys' >/dev/null 2>&1; then
    PYTHON_BIN=python3
elif python -c 'import sys' >/dev/null 2>&1; then
    PYTHON_BIN=python
else
    echo "ERROR: Python 3 is required to assemble the ABI JSON." >&2
    exit 1
fi

TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT

for contract in "${ABI_CONTRACTS[@]}"; do
    wasm_path="$WASM_DIR/$contract.wasm"
    if [[ ! -f "$wasm_path" ]]; then
        echo "ERROR: Missing WASM artifact for '$contract': $wasm_path" >&2
        echo "Build contract artifacts with 'make build' first." >&2
        exit 1
    fi

    echo "Generating ABI for $contract..." >&2
    if ! "$STELLAR_BIN" -q contract info interface --wasm "$wasm_path" --output json > "$TEMP_DIR/$contract.json"; then
        echo "ERROR: Stellar CLI could not read the interface for '$contract'." >&2
        exit 1
    fi
done

"$PYTHON_BIN" - "$TEMP_DIR" "$OUTPUT_FILE" "${ABI_CONTRACTS[@]}" <<'PY'
import json
import os
import sys

temp_dir, output_path, *contracts = sys.argv[1:]
interfaces = {}
for name in contracts:
    try:
        with open(os.path.join(temp_dir, f"{name}.json"), encoding="utf-8") as stream:
            interfaces[name] = json.load(stream)
    except (OSError, json.JSONDecodeError) as error:
        print(f"ERROR: Invalid interface JSON for '{name}': {error}", file=sys.stderr)
        sys.exit(1)

os.makedirs(os.path.dirname(os.path.abspath(output_path)), exist_ok=True)
with open(output_path, "w", encoding="utf-8", newline="\n") as stream:
    json.dump({"contracts": interfaces}, stream, indent=2, sort_keys=True, ensure_ascii=False)
    stream.write("\n")
PY

echo "Successfully generated $OUTPUT_FILE" >&2
