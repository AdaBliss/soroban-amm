#!/usr/bin/env bash
#
# Build every deployable contract in the workspace for wasm32v1-none.
#
# This is the single source of truth for which crates are excluded from the
# wasm build. It is called by .github/workflows/ci.yml, .github/workflows/
# release.yml, and the Makefile's `build` target, so the three can no longer
# drift apart.
# The ABI generator intentionally uses the narrower explicit contract list in
# scripts/abi_contracts.sh rather than every artifact this build produces.
#
# Cargo always builds against the checked-in lockfile. Any extra arguments
# are forwarded to cargo; a repeated --locked is removed below.
set -euo pipefail

# Crates that produce no deployable wasm artifact:
# - amm_fuzz, integration_tests, benches: test/bench harnesses, not contracts.
# - soroban_amm_simulator: off-chain CLI. Its host-only dependencies (clap,
#   csv, rand/getrandom) have no wasm32v1-none support and fail to compile,
#   so building it for wasm breaks the whole workspace build.
EXCLUDE_PACKAGES=(
  amm_fuzz
  integration_tests
  benches
  soroban_amm_simulator
  # Examples and Rust libraries are useful workspace members, but are not
  # deployable Soroban contracts and must not produce release WASM.
  flash_loan_receiver
  soroban_amm_sdk
)

exclude_flags=()
for pkg in "${EXCLUDE_PACKAGES[@]}"; do
  exclude_flags+=(--exclude "$pkg")
done

cargo_args=()
for arg in "$@"; do
  [[ "$arg" == "--locked" ]] || cargo_args+=("$arg")
done

set -x
# Capture stderr so we can scan it for linker signature-mismatch warnings,
# which indicate that two contract crates are linked together into one WASM.
# Those warnings are not fatal by default but produce broken binaries
# (neither initialize implementation is exported). Treat them as errors.
LINKER_STDERR=$(mktemp)
cargo build --locked --release --target wasm32v1-none --workspace "${exclude_flags[@]}" "${cargo_args[@]}" \
  2> >(tee "$LINKER_STDERR" >&2)

if grep -q "function signature mismatch" "$LINKER_STDERR"; then
  echo "" >&2
  echo "ERROR: Linker detected a function signature mismatch — two contract crates" >&2
  echo "       are being linked into the same WASM binary. This produces a broken" >&2
  echo "       artifact with missing or colliding exports. Fix the dependency" >&2
  echo "       (see contracts/pool_interfaces/src/lib.rs for the correct pattern)." >&2
  rm -f "$LINKER_STDERR"
  exit 1
fi

rm -f "$LINKER_STDERR"
