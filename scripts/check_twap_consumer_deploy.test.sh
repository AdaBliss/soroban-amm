#!/usr/bin/env bash
# Exercise the twap_consumer deploy verification (#1159) against a stub `stellar`
# CLI, so no network or key is needed. The stub answers
# `stellar contract invoke ... -- <function>` from files in $STUB_DIR.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/bin"

cat > "$scratch/bin/stellar" <<'STUB'
#!/usr/bin/env bash
fn=""
seen=0
for arg in "$@"; do
  if [[ $seen == 1 ]]; then fn="$arg"; break; fi
  if [[ "$arg" == "--" ]]; then seen=1; fi
done
echo "$fn" >> "$STUB_DIR/calls"
if [[ -f "$STUB_DIR/$fn.out" ]]; then
  cat "$STUB_DIR/$fn.out"
  exit 0
fi
echo "error: simulation failed" >&2
exit 1
STUB
chmod +x "$scratch/bin/stellar"

# 56-character strkey-shaped addresses (G/C + 55 characters of [A-Z2-7]).
rep() { printf "$1%.0s" $(seq 1 55); }
KEEPER="G$(rep A)"
OTHER="G$(rep C)"

count=0
new_case() {
  count=$((count + 1))
  export STUB_DIR="$scratch/case$count"
  mkdir "$STUB_DIR"
}

# verify: runs the real verify_twap_consumer in a subshell so a `die` cannot end
# this script.
verify() {
  (
    export PATH="$scratch/bin:$PATH"
    export NETWORK=testnet SOURCE_ACCOUNT=stub VERIFY_RETRY_DELAY=0
    # shellcheck source=/dev/null
    source "$here/deploy/common.sh"
    # shellcheck source=/dev/null
    source "$here/deploy/twap_consumer.sh"
    verify_twap_consumer CTWAPCONSUMER "$KEEPER"
  ) > "$scratch/output" 2>&1
}

accepts() {
  if ! verify; then
    echo "FAIL: $1 was rejected" >&2
    cat "$scratch/output" >&2
    exit 1
  fi
  grep -q "verified twap_consumer CTWAPCONSUMER admin=$KEEPER" "$scratch/output"
  echo "PASS: $1"
}

rejects() {
  if verify; then
    echo "FAIL: $1 was accepted" >&2
    exit 1
  fi
  grep -q "$2" "$scratch/output" || {
    echo "FAIL: $1 failed with an unexpected message:" >&2
    cat "$scratch/output" >&2
    exit 1
  }
  echo "PASS: $1"
}

new_case
echo "$KEEPER" > "$STUB_DIR/get_keeper.out"
accepts "matching keeper"

new_case
echo "$OTHER" > "$STUB_DIR/get_keeper.out"
rejects "front-run keeper" "admin mismatch"

new_case
rejects "uninitialized contract" "returned nothing after retries"
test "$(grep -c '^get_keeper$' "$STUB_DIR/calls")" = 5

new_case
echo "not an address" > "$STUB_DIR/get_keeper.out"
rejects "get_keeper without an address" "returned no address"

echo "all twap_consumer deploy verification checks passed"
