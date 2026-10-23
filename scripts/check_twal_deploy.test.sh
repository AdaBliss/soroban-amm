#!/usr/bin/env bash
# Verify TWAL deployment rejects an initialized contract with the wrong keeper.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/stub"

cat > "$scratch/bin/stellar" <<'STUB'
#!/usr/bin/env bash
fn=""
seen=0
for arg in "$@"; do
  if [[ $seen == 1 ]]; then fn="$arg"; break; fi
  if [[ "$arg" == "--" ]]; then seen=1; fi
done
cat "$STUB_DIR/$fn.out"
STUB
chmod +x "$scratch/bin/stellar"

export PATH="$scratch/bin:$PATH"
export STUB_DIR="$scratch/stub"
export NETWORK=testnet SOURCE_ACCOUNT=stub

(
  source "$here/deploy/common.sh"
  source "$here/deploy/twal_consumer.sh"

  printf '%s\n' 'GEXPECTED' > "$STUB_DIR/get_keeper.out"
  verify_twal_consumer_keeper CTWAL GEXPECTED

  printf '%s\n' 'GEXPECTED-extra' > "$STUB_DIR/get_keeper.out"
  if (verify_twal_consumer_keeper CTWAL GEXPECTED) > "$scratch/mismatch.out" 2>&1; then
    echo 'FAIL: keeper substring mismatch was accepted' >&2
    exit 1
  fi
  grep -q 'keeper mismatch' "$scratch/mismatch.out"
)

echo 'TWAL deployment keeper verification checks passed'