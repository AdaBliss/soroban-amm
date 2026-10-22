#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT_DIR/scripts/release_manifest.sh"
BUILD_SCRIPT="$ROOT_DIR/scripts/build_workspace.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if command -v python3 >/dev/null 2>&1 && python3 -c 'import sys' >/dev/null 2>&1; then
  PYTHON_CMD=python3
else
  PYTHON_CMD=python
fi

wasms=(
  amm.wasm batch_auction.wasm batch_router.wasm cl_position_nft.wasm
  concentrated_liquidity.wasm dex_aggregator.wasm factory.wasm governance.wasm
  incentive_campaigns.wasm oracle_aggregator.wasm pol_vesting.wasm
  reserve_manager.wasm router.wasm staking.wasm token.wasm twal_consumer.wasm
  twap_consumer.wasm v2_to_v3_migration.wasm
)

# Artifact-set regression: exact deployables pass; missing files, the example,
# and the SDK library all fail.
mkdir -p "$tmp/artifacts"
for wasm in "${wasms[@]}"; do printf '%s' "$wasm" > "$tmp/artifacts/$wasm"; done
bash "$SCRIPT" check-artifacts "$tmp/artifacts"
rm "$tmp/artifacts/amm.wasm"
if bash "$SCRIPT" check-artifacts "$tmp/artifacts" >/dev/null 2>&1; then
  echo "artifact set unexpectedly accepted a missing artifact" >&2; exit 1
fi
printf 'amm.wasm' > "$tmp/artifacts/amm.wasm"
cp "$tmp/artifacts/amm.wasm" "$tmp/artifacts/flash_loan_receiver.wasm"
if bash "$SCRIPT" check-artifacts "$tmp/artifacts" >/dev/null 2>&1; then
  echo "artifact set unexpectedly accepted flash_loan_receiver.wasm" >&2; exit 1
fi
rm "$tmp/artifacts/flash_loan_receiver.wasm"
cp "$tmp/artifacts/amm.wasm" "$tmp/artifacts/soroban_amm_sdk.wasm"
if bash "$SCRIPT" check-artifacts "$tmp/artifacts" >/dev/null 2>&1; then
  echo "artifact set unexpectedly accepted soroban_amm_sdk.wasm" >&2; exit 1
fi
rm "$tmp/artifacts/soroban_amm_sdk.wasm"
printf 'cargo dep info' > "$tmp/artifacts/amm.d"
printf 'cargo library' > "$tmp/artifacts/libamm.rlib"
bash "$SCRIPT" check-artifacts "$tmp/artifacts"
if bash "$SCRIPT" check-strict-artifacts "$tmp/artifacts" >/dev/null 2>&1; then
  echo "strict artifact check unexpectedly accepted Cargo byproducts" >&2; exit 1
fi
rm "$tmp/artifacts/amm.d" "$tmp/artifacts/libamm.rlib"
echo "ok: release artifact set is exact"

# Lock regression: the build wrapper must pass --locked by default. A fake
# Cargo records the actual args and succeeds only when the flag is present.
mkdir -p "$tmp/bin"
cat > "$tmp/bin/cargo" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" > "$CARGO_ARGS_LOG"
[[ " $* " == *" --locked "* ]] || exit 1
if [[ "${MOCK_LOCK_MISMATCH:-}" == "1" ]]; then
  echo "lockfile needs updating" >&2
  exit 101
fi
SH
chmod +x "$tmp/bin/cargo"
CARGO_ARGS_LOG="$tmp/cargo-args" PATH="$tmp/bin:$PATH" bash "$BUILD_SCRIPT" >/dev/null 2>&1
grep -q -- '--locked' "$tmp/cargo-args"
CARGO_ARGS_LOG="$tmp/cargo-args" PATH="$tmp/bin:$PATH" bash "$BUILD_SCRIPT" --locked >/dev/null 2>&1
[[ "$(grep -o -- '--locked' "$tmp/cargo-args" | wc -l | tr -d ' ')" == "1" ]]
if CARGO_ARGS_LOG="$tmp/cargo-args" MOCK_LOCK_MISMATCH=1 PATH="$tmp/bin:$PATH" bash "$BUILD_SCRIPT" >/dev/null 2>&1; then
  echo "workspace build unexpectedly accepted a stale lockfile" >&2; exit 1
fi
echo "ok: workspace build defaults to --locked"

# Manifest regression: generated metadata hashes match sha256sum -c and record
# the release identity and both pinned tool versions.
bash "$SCRIPT" write-checksums "$tmp/artifacts"
bash "$SCRIPT" write-manifest "$tmp/artifacts" abc123 v1.2.3 1.98.1 27.1.0
bash "$SCRIPT" check-release-assets "$tmp/artifacts"
(cd "$tmp/artifacts" && sha256sum -c sha256sums.txt >/dev/null)
"$PYTHON_CMD" - "$tmp/artifacts/manifest.json" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as f:
    manifest = json.load(f)
assert manifest["commit"] == "abc123"
assert manifest["tag"] == "v1.2.3"
assert manifest["rust_toolchain"] == "1.98.1"
assert manifest["stellar_cli_version"] == "27.1.0"
assert len(manifest["artifacts"]) == 18
assert all(len(item["sha256"]) == 64 and item["size_bytes"] > 0 for item in manifest["artifacts"])
assert not {"flash_loan_receiver.wasm", "soroban_amm_sdk.wasm"} & {item["name"] for item in manifest["artifacts"]}
PY
echo "ok: manifest records release metadata and checksummed artifacts"
