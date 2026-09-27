#!/usr/bin/env bash
# Soroban AMM — full-protocol deployment orchestrator
#
# Deploys and initializes every contract in dependency order, persisting each
# address and step to $DEPLOY_ENV as it completes so the run is idempotent
# and resumable (kill mid-run and re-run to continue). Verification reads
# state back after each initialization — a zero exit code does NOT mean success.
#
# Usage:
#   scripts/deploy.sh [network] [--only a,b] [--skip c,d] [--force] [--help]
#   NETWORK=testnet SOURCE_ACCOUNT=mykey scripts/deploy.sh --only factory,pools
#
# Helpers are importable (sourceable):
#   source scripts/deploy.sh  # or source scripts/deploy/common.sh
#   # then call deploy_tokens, deploy_factory, etc. individually
#
# WASM target: wasm32v1-none (the only supported Soroban target).
# All WASM artifacts are expected under target/wasm32v1-none/release/.

set -Eeuo pipefail

# ── Resolve ROOT_DIR even when sourced ─────────────────────────────────────
if [[ -n "${BASH_SOURCE[0]:-}" ]]; then
  ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
else
  ROOT_DIR="$(pwd)"
fi

# Only set defaults if not already set (allows caller to override before sourcing)
: "${NETWORK:=testnet}"
: "${SOURCE_ACCOUNT:=soroban-amm-deployer}"
: "${DEPLOY_ENV:=${ROOT_DIR}/.soroban-amm.deploy.env}"
: "${ADMIN_ADDRESS:=}"
: "${FEE_RECIPIENT:=}"
: "${PROTOCOL_FEE_BPS:=0}"

WASM_TARGET="wasm32v1-none"

# ── Source common helpers ──────────────────────────────────────────────────
# When this file is sourced, ROOT_DIR/NETWORK etc. are already set above so
# common.sh can use them.

# shellcheck disable=SC1091
if [[ -f "${ROOT_DIR}/scripts/deploy/common.sh" ]]; then
  source "${ROOT_DIR}/scripts/deploy/common.sh"
else
  # Fallback: minimal log/die if common.sh not yet present (e.g. during bootstrap)
  log() { printf '[deploy] %s\n' "$*" >&2; }
  die() { printf '[deploy][error] %s\n' "$*" >&2; exit 1; }
fi

# Source per-contract modules (all are sourceable and idempotent to source).
# ALL_CONTRACTS in common.sh is the single list of deploy steps; a step with
# no module is an error rather than a silent skip, and
# scripts/check_deploy_scripts.sh fails CI when a deployable workspace
# contract is missing from that list.
for mod in "${ALL_CONTRACTS[@]}"; do
  if [[ ! -f "${ROOT_DIR}/scripts/deploy/${mod}.sh" ]]; then
    die "no deploy module for '${mod}': expected scripts/deploy/${mod}.sh"
  fi
  # shellcheck disable=SC1090
  source "${ROOT_DIR}/scripts/deploy/${mod}.sh"
done

# ── Argument parsing ───────────────────────────────────────────────────────
ONLY_RAW=""
SKIP_RAW=""
FORCE=false
POSITIONAL_NETWORK=""

print_help() {
  cat <<'HELP'
Soroban AMM deploy — full protocol deployment to Stellar (testnet/mainnet)

Usage:
  scripts/deploy.sh [network] [options]

Arguments:
  network                 Stellar network name (testnet, mainnet, futurenet).
                          Also reads $NETWORK / $STELLAR_NETWORK. Default: testnet.

Options:
  --only LIST             Deploy only these contracts (comma-separated).
                          Example: --only factory,pools,governance
  --skip LIST             Skip these contracts.
                          Example: --skip staking,incentive_campaigns
  --force                 Re-deploy and re-initialize even if already persisted.
                          Without --force, a re-run is a no-op for completed steps.
  --help, -h              Show this help.

Available contract names (in deployment order):
  token, amm, concentrated_liquidity, factory, pools,
  governance, oracle_aggregator, twap_consumer, twal_consumer,
  staking, incentive_campaigns, pol_vesting, reserve_manager,
  router, dex_aggregator, batch_auction, batch_router,
  cl_position_nft, v2_to_v3_migration

  Aliases: amm covers the AMM WASM artifact (pools via factory).
           concentrated_liquidity is also "cl".

Persistence:
  Every deployed address and init marker is appended to $DEPLOY_ENV
  immediately after it completes. Kill the script mid-run and re-run —
  it resumes from where it left off. A completed deployment re-run
  is a no-op unless --force is passed.

Verification:
  After each initialization the script reads state back (get_info,
  get_params, etc.) and asserts expected values.

Examples:
  scripts/deploy.sh testnet
  scripts/deploy.sh --only factory,pools
  scripts/deploy.sh --skip governance,staking
  NETWORK=mainnet SOURCE_ACCOUNT=mainnet-deployer scripts/deploy.sh --force

Runbook:
  See docs/deployment-runbook.md for prerequisites, deployment order,
  per-contract parameters, verification, upgrades, and emergency procedures.

HELP
}

parse_args() {
  local args=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --only)
        ONLY_RAW="${2:-}"
        shift 2
        ;;
      --only=*)
        ONLY_RAW="${1#--only=}"
        shift
        ;;
      --skip)
        SKIP_RAW="${2:-}"
        shift 2
        ;;
      --skip=*)
        SKIP_RAW="${1#--skip=}"
        shift
        ;;
      --force)
        FORCE=true
        shift
        ;;
      --help|-h)
        print_help
        exit 0
        ;;
      --network)
        NETWORK="${2:-testnet}"
        shift 2
        ;;
      --network=*)
        NETWORK="${1#--network=}"
        shift
        ;;
      --*)
        die "unknown option: $1 (see --help)"
        ;;
      *)
        # Positional arg — treat first as network if it looks like a network name
        if [[ -z "$POSITIONAL_NETWORK" ]] && [[ "$1" =~ ^(testnet|mainnet|futurenet|local|standalone)$ ]]; then
          POSITIONAL_NETWORK="$1"
        else
          args+=("$1")
        fi
        shift
        ;;
    esac
  done

  if [[ -n "$POSITIONAL_NETWORK" ]]; then
    NETWORK="$POSITIONAL_NETWORK"
  fi
  # Backward compat: if first positional arg was not a network name but caller
  # passed "testnet" as $1, it's already handled above.

  # Normalize filters to lowercase underscore form, split on comma
  ONLY_FILTER=()
  SKIP_FILTER=()
  if [[ -n "$ONLY_RAW" ]]; then
    IFS=',' read -ra _only_parts <<< "$ONLY_RAW"
    for p in "${_only_parts[@]}"; do
      p=$(echo "$p" | tr '[:upper:]' '[:lower:]' | tr '-' '_' | xargs)
      # Alias: cl -> concentrated_liquidity
      if [[ "$p" == "cl" ]]; then p="concentrated_liquidity"; fi
      if [[ -n "$p" ]]; then ONLY_FILTER+=("$p"); fi
    done
  fi
  if [[ -n "$SKIP_RAW" ]]; then
    IFS=',' read -ra _skip_parts <<< "$SKIP_RAW"
    for p in "${_skip_parts[@]}"; do
      p=$(echo "$p" | tr '[:upper:]' '[:lower:]' | tr '-' '_' | xargs)
      if [[ "$p" == "cl" ]]; then p="concentrated_liquidity"; fi
      if [[ -n "$p" ]]; then SKIP_FILTER+=("$p"); fi
    done
  fi

  export NETWORK FORCE
  # Export for common.sh filter functions
  export ONLY_RAW SKIP_RAW
}

# ── Main orchestration ─────────────────────────────────────────────────────

main() {
  parse_args "$@"

  log "network=$NETWORK source=$SOURCE_ACCOUNT env=$DEPLOY_ENV force=$FORCE"
  if [[ ${#ONLY_FILTER[@]} -gt 0 ]]; then log "only: ${ONLY_FILTER[*]}"; fi
  if [[ ${#SKIP_FILTER[@]} -gt 0 ]]; then log "skip: ${SKIP_FILTER[*]}"; fi

  require_cmd stellar

  # Load any existing deployment state (for resumability)
  load_env

  generate_and_fund_source
  SOURCE_PUBLIC_KEY="$(stellar keys address "$SOURCE_ACCOUNT")"
  persist_var "NETWORK" "$NETWORK"
  persist_var "SOURCE_ACCOUNT" "$SOURCE_ACCOUNT"
  persist_var "SOURCE_PUBLIC_KEY" "$SOURCE_PUBLIC_KEY"

  if [[ -z "${ADMIN_ADDRESS:-}" ]]; then
    ADMIN_ADDRESS="$SOURCE_PUBLIC_KEY"
    log "ADMIN_ADDRESS not set — using source public key: $ADMIN_ADDRESS"
  fi
  if [[ -z "${FEE_RECIPIENT:-}" ]]; then
    FEE_RECIPIENT="$SOURCE_PUBLIC_KEY"
  fi
  persist_var "ADMIN_ADDRESS" "$ADMIN_ADDRESS"
  persist_var "FEE_RECIPIENT" "$FEE_RECIPIENT"
  persist_var "PROTOCOL_FEE_BPS" "$PROTOCOL_FEE_BPS"
  export ADMIN_ADDRESS FEE_RECIPIENT PROTOCOL_FEE_BPS SOURCE_PUBLIC_KEY

  # ── Build WASM artifacts (correct target: wasm32v1-none) ────────────────
  if [[ "$FORCE" == true ]] || ! ls "${ROOT_DIR}/target/${WASM_TARGET}/release/"*.wasm >/dev/null 2>&1; then
    # Determine if we should build: missing files or --force
    local need_build=false
    for wasm in "${ROOT_DIR}/target/${WASM_TARGET}/release/token.wasm" "${ROOT_DIR}/target/${WASM_TARGET}/release/amm.wasm" "${ROOT_DIR}/target/${WASM_TARGET}/release/factory.wasm"; do
      if [[ ! -f "$wasm" ]]; then need_build=true; break; fi
    done
    if [[ "$FORCE" == true ]]; then need_build=true; fi
    if [[ "$need_build" == true ]]; then
      require_cmd cargo
      log "building release WASM artifacts (target ${WASM_TARGET})"
      CURRENT_CONTRACT="build"
      # Shared with ci.yml, release.yml and the Makefile so the crates that
      # produce no deployable wasm are excluded here too.
      CURRENT_STEP="bash scripts/build_workspace.sh"
      (cd "$ROOT_DIR" && bash scripts/build_workspace.sh) || die "cargo build failed"
    fi
  else
    log "WASM artifacts already present — skipping build (use --force to rebuild)"
  fi

  # Ensure wasm32v1-none artifacts are what we deploy; fail fast if wrong target was built elsewhere
  if compgen -G "${ROOT_DIR}/target/wasm32-unknown-unknown/release/*.wasm" >/dev/null 2>&1; then
    warn "found wasm32-unknown-unknown artifacts — these are the WRONG target and will not be used (expected wasm32v1-none)"
  fi

  # ── Deploy in dependency order ──────────────────────────────────────────
  # 1. Underlying tokens (no dependencies)
  deploy_tokens

  # 2. Factory (needs WASM hashes; uploads token/amm/cl wasm inside)
  deploy_factory

  # 3. Pools via factory (needs factory + tokens)
  deploy_pools

  # 4. Governance (needs AMM pool + LP token)
  deploy_governance

  # 5. Oracle stack
  deploy_oracle_aggregator
  deploy_twap_consumer
  deploy_twal_consumer

  # 6. Staking & incentives
  deploy_staking
  deploy_incentive_campaigns
  deploy_pol_vesting
  deploy_reserve_manager

  # 7. Routing (batch_router fronts batch_auction, so it follows it)
  deploy_router
  deploy_dex_aggregator
  deploy_batch_auction
  deploy_batch_router

  # 8. NFTs & migration
  deploy_cl_position_nft
  deploy_v2_to_v3_migration

  # ── Summary ─────────────────────────────────────────────────────────────
  log "wrote deployment env to $DEPLOY_ENV"
  log "deployment complete — persisted addresses:"

  # Print summary from DEPLOY_ENV (only contract addresses)
  if [[ -f "$DEPLOY_ENV" ]]; then
    grep -E '^export .*_CONTRACT_ID=' "$DEPLOY_ENV" 2>/dev/null | sed 's/export /  /' >&2 || true
    grep -E '^export .*_WASM_HASH=' "$DEPLOY_ENV" 2>/dev/null | sed 's/export /  /' >&2 || true
  fi

  # Also print to stdout for PR evidence
  echo "=== Deployment addresses (network: $NETWORK) ==="
  if [[ -f "$DEPLOY_ENV" ]]; then
    grep -E '^export .*_(CONTRACT_ID|WASM_HASH)=' "$DEPLOY_ENV" 2>/dev/null | sed 's/export //' || true
  fi
  echo "=== Verification: all post-deploy checks ran (see log above) ==="
  echo "Env file: $DEPLOY_ENV"
}

# ── Entrypoint guard — only run main if executed, not when sourced ────────
# This keeps helpers importable for e2e.sh reuse without triggering a deploy.
if [[ "${BASH_SOURCE[0]:-}" == "${0}" ]]; then
  main "$@"
fi
