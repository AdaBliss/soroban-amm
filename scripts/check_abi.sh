#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
    echo "Usage: bash scripts/check_abi.sh [--worktree | --reference FILE]" >&2
    echo "Default comparison target: docs/abi.json from HEAD." >&2
}

REFERENCE_MODE=head
REFERENCE_FILE=""
case "${1:-}" in
    "") ;;
    --worktree)
        REFERENCE_MODE=worktree
        ;;
    --reference)
        if [[ $# -ne 2 ]]; then
            usage
            exit 2
        fi
        REFERENCE_MODE=file
        REFERENCE_FILE="$2"
        ;;
    *)
        usage
        exit 2
        ;;
esac

TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT
GENERATED_FILE="$TEMP_DIR/abi.json"
EXPECTED_FILE="$TEMP_DIR/expected.json"

ABI_OUTPUT_FILE="$GENERATED_FILE" bash "$SCRIPT_DIR/generate_abi.sh" "$GENERATED_FILE"

case "$REFERENCE_MODE" in
    head)
        if ! git -C "$ROOT_DIR" show HEAD:docs/abi.json > "$EXPECTED_FILE"; then
            echo "ERROR: Could not read docs/abi.json from HEAD." >&2
            exit 1
        fi
        ;;
    worktree)
        cp "$ROOT_DIR/docs/abi.json" "$EXPECTED_FILE"
        ;;
    file)
        cp "$REFERENCE_FILE" "$EXPECTED_FILE"
        ;;
esac

if cmp -s "$EXPECTED_FILE" "$GENERATED_FILE"; then
    echo "Success: docs/abi.json matches the interface generated from WASM artifacts."
    exit 0
fi

echo "ERROR: docs/abi.json is out of date." >&2
echo "Regenerate it with 'make abi', then commit docs/abi.json." >&2
python_command=python3
if ! "$python_command" -c 'import sys' >/dev/null 2>&1; then
    python_command=python
fi
"$python_command" "$SCRIPT_DIR/compare_abi.py" "$EXPECTED_FILE" "$GENERATED_FILE" >&2
exit 1
