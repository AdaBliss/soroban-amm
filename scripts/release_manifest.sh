#!/usr/bin/env bash
# Validate the deployable WASM set and write deterministic release metadata.
set -euo pipefail

EXPECTED_WASMS=(
  amm.wasm
  batch_auction.wasm
  batch_router.wasm
  cl_position_nft.wasm
  concentrated_liquidity.wasm
  dex_aggregator.wasm
  factory.wasm
  governance.wasm
  incentive_campaigns.wasm
  oracle_aggregator.wasm
  pol_vesting.wasm
  reserve_manager.wasm
  router.wasm
  staking.wasm
  token.wasm
  twal_consumer.wasm
  twap_consumer.wasm
  v2_to_v3_migration.wasm
)

usage() {
  echo "Usage: $0 check-artifacts DIR | check-strict-artifacts DIR | check-release-assets DIR | write-checksums DIR | write-manifest DIR COMMIT TAG RUST_TOOLCHAIN STELLAR_CLI_VERSION" >&2
  exit 2
}

[[ $# -ge 2 ]] || usage
command="$1"
dir="${2%/}"
[[ -d "$dir" ]] || { echo "release artifact directory not found: $dir" >&2; exit 1; }
if command -v python3 >/dev/null 2>&1 && python3 -c 'import sys' >/dev/null 2>&1; then
  PYTHON_CMD=python3
elif command -v python >/dev/null 2>&1 && python -c 'import sys' >/dev/null 2>&1; then
  PYTHON_CMD=python
else
  echo "Python 3 is required to generate release manifests" >&2
  exit 1
fi

check_artifacts() {
  local strict="$1" include_checksums="$2" actual expected
  if [[ "$strict" == "true" ]]; then
    actual="$(find "$dir" -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)"
  else
    actual="$(find "$dir" -maxdepth 1 -type f -name '*.wasm' -printf '%f\n' | LC_ALL=C sort)"
  fi
  expected="$(printf '%s\n' "${EXPECTED_WASMS[@]}" | LC_ALL=C sort)"
  if [[ "$include_checksums" == "true" || "$include_checksums" == "assets" ]]; then
    expected="$(printf '%s\n' "$expected" sha256sums.txt | LC_ALL=C sort)"
  fi
  if [[ "$include_checksums" == "assets" ]]; then
    expected="$(printf '%s\n' "$expected" manifest.json | LC_ALL=C sort)"
  fi
  if [[ "$actual" != "$expected" ]]; then
    echo "release artifact set mismatch in $dir" >&2
    echo "Expected:" >&2; printf '  %s\n' "${EXPECTED_WASMS[@]}" >&2
    echo "Found:" >&2; if [[ -n "$actual" ]]; then sed 's/^/  /' <<< "$actual" >&2; else echo '  (none)' >&2; fi
    exit 1
  fi
}

case "$command" in
  check-artifacts)
    [[ $# -eq 2 ]] || usage
    check_artifacts false false
    ;;
  check-strict-artifacts)
    [[ $# -eq 2 ]] || usage
    check_artifacts true false
    ;;
  check-release-assets)
    [[ $# -eq 2 ]] || usage
    check_artifacts true assets
    ;;
  write-checksums)
    [[ $# -eq 2 ]] || usage
    check_artifacts false false
    (cd "$dir" && sha256sum "${EXPECTED_WASMS[@]}" > sha256sums.txt)
    ;;
  write-manifest)
    [[ $# -eq 6 ]] || usage
    check_artifacts true true
    [[ -f "$dir/sha256sums.txt" ]] || { echo "sha256sums.txt must be generated first" >&2; exit 1; }
    export RELEASE_DIR="$dir" RELEASE_COMMIT="$3" RELEASE_TAG="$4"
    export RELEASE_RUST_TOOLCHAIN="$5" RELEASE_STELLAR_CLI_VERSION="$6"
    "$PYTHON_CMD" - <<'PY'
import hashlib
import json
import os
from pathlib import Path

directory = Path(os.environ["RELEASE_DIR"])
wasms = sorted(directory.glob("*.wasm"))
artifacts = []
for path in wasms:
    data = path.read_bytes()
    artifacts.append({
        "name": path.name,
        "size_bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    })

manifest = {
    "commit": os.environ["RELEASE_COMMIT"],
    "tag": os.environ["RELEASE_TAG"],
    "rust_toolchain": os.environ["RELEASE_RUST_TOOLCHAIN"],
    "stellar_cli_version": os.environ["RELEASE_STELLAR_CLI_VERSION"],
    "artifacts": artifacts,
}
(directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY
    ;;
  *) usage ;;
esac
