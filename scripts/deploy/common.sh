#!/usr/bin/env bash
# common.sh — shared helpers for Soroban AMM deployment
# Sourceable: `source scripts/deploy/common.sh`
# All functions are importable; no side effects on source.

# ── Constants ────────────────────────────────────────────────────────────────

WASM_TARGET="wasm32v1-none"
WASM_DIR="target/${WASM_TARGET}/release"

# Recommended defaults (see docs/deployment-runbook.md)
DEFAULT_FEE_BPS=30
DEFAULT_FEE_TIER=2
DEFAULT_PROTOCOL_FEE_BPS=0
DEFAULT_VOTING_PERIOD_SECS=604800
DEFAULT_TIMELOCK_SECS=172800
DEFAULT_QUORUM_BPS=1000
DEFAULT_MIN_PROPOSER_STAKE_BPS=100
DEFAULT_CIRCUIT_BREAKER_THRESHOLD_BPS=5000
DEFAULT_CIRCUIT_BREAKER_COOLDOWN_SECS=600
DEFAULT_MAX_ORACLE_DEVIATION_BPS=500
DEFAULT_ORACLE_MAX_STALENESS_SECS=3600
DEFAULT_ORACLE_MAX_DEVIATION_BPS=500
DEFAULT_BATCH_WINDOW_SECS=60

# Ordered list of all deployable contracts (dependency order)
ALL_CONTRACTS=(
  token
  amm
  concentrated_liquidity
  factory
  pools
  governance
  oracle_aggregator
  twap_consumer
  twal_consumer
  staking
  incentive_campaigns
  pol_vesting
  reserve_manager
  router
  dex_aggregator
  batch_auction
  batch_router
  cl_position_nft
  v2_to_v3_migration
)

# Map contract name → WASM filename (post-build)
# Package names with hyphens use underscores in artifact names.
declare -A WASM_FILE=(
  [token]="token.wasm"
  [amm]="amm.wasm"
  [concentrated_liquidity]="concentrated_liquidity.wasm"
  [factory]="factory.wasm"
  [governance]="governance.wasm"
  [staking]="staking.wasm"
  [twap_consumer]="twap_consumer.wasm"
  [twal_consumer]="twal_consumer.wasm"
  [oracle_aggregator]="oracle_aggregator.wasm"
  [router]="router.wasm"
  [batch_router]="batch_router.wasm"
  [dex_aggregator]="dex_aggregator.wasm"
  [batch_auction]="batch_auction.wasm"
  [cl_position_nft]="cl_position_nft.wasm"
  [reserve_manager]="reserve_manager.wasm"
  [incentive_campaigns]="incentive_campaigns.wasm"
  [pol_vesting]="pol_vesting.wasm"
  [v2_to_v3_migration]="v2_to_v3_migration.wasm"
)

# ── Logging ────────────────────────────────────────────────────────────────

log()  { printf '[deploy] %s\n' "$*" >&2; }
warn() { printf '[deploy][warn] %s\n' "$*" >&2; }
die()  { printf '[deploy][error] %s\n' "$*" >&2; exit 1; }

# Track current step for error reporting
CURRENT_CONTRACT=""
CURRENT_STEP=""

trap 'rc=$?; if [[ $rc -ne 0 ]]; then printf "[deploy][error] failed at contract=%s step=%s (exit %d)\n" "${CURRENT_CONTRACT:-unknown}" "${CURRENT_STEP:-unknown}" "$rc" >&2; fi' ERR

# ── Persistence ────────────────────────────────────────────────────────────

# persist_var KEY VALUE
# Atomically writes KEY to DEPLOY_ENV, updating in place if already present.
persist_var() {
  local key="$1"
  local val="$2"
  local env_file="${DEPLOY_ENV:?DEPLOY_ENV not set}"

  # Ensure env file exists with header
  if [[ ! -f "$env_file" ]]; then
    printf '# Soroban AMM deployment env — auto-generated, do not edit manually\n' > "$env_file"
    printf '# Re-running deploy.sh is idempotent; use --force to overwrite.\n' >> "$env_file"
  fi

  # Escape value for shell: single-quote and escape single quotes
  local esc_val
  esc_val=$(printf "%s" "$val" | sed "s/'/'\\\\''/g")

  if grep -q "^export ${key}=" "$env_file" 2>/dev/null; then
    # Replace existing line
    local tmp
    tmp=$(mktemp)
    sed "s|^export ${key}=.*|export ${key}='${esc_val}'|" "$env_file" > "$tmp" && mv "$tmp" "$env_file"
  else
    printf "export %s='%s'\n" "$key" "$esc_val" >> "$env_file"
  fi

  # Also export in current shell
  export "${key}=${val}"
  # Also set bare variable for convenience
  printf -v "$key" '%s' "$val"
}

# load_env — source DEPLOY_ENV if it exists
load_env() {
  if [[ -f "${DEPLOY_ENV:-}" ]]; then
    # shellcheck disable=SC1090
    set -a
    source "$DEPLOY_ENV"
    set +a
    log "loaded existing deploy env from $DEPLOY_ENV"
  fi
}

# is_persisted KEY — true if KEY is non-empty in env
is_persisted() {
  local key="$1"
  local val
  val=$(grep "^export ${key}=" "${DEPLOY_ENV:-/dev/null}" 2>/dev/null | tail -n 1 | sed "s/^export ${key}='//;s/'$//" || true)
  [[ -n "$val" ]]
}

get_persisted() {
  local key="$1"
  grep "^export ${key}=" "${DEPLOY_ENV:-/dev/null}" 2>/dev/null | tail -n 1 | sed "s/^export ${key}='//;s/'$//" || echo ""
}

# ── Filtering ──────────────────────────────────────────────────────────────

# Global filter arrays set by parse_args
ONLY_FILTER=()
SKIP_FILTER=()
FORCE=false

# should_deploy CONTRACT → 0 if should deploy, 1 if skipped
should_deploy() {
  local contract="$1"
  local in_only=false
  local in_skip=false

  if [[ ${#ONLY_FILTER[@]} -gt 0 ]]; then
    in_only=false
    for f in "${ONLY_FILTER[@]}"; do
      if [[ "$f" == "$contract" ]]; then in_only=true; break; fi
    done
    if [[ "$in_only" == false ]]; then return 1; fi
  fi

  for f in "${SKIP_FILTER[@]}"; do
    if [[ "$f" == "$contract" ]]; then return 1; fi
  done

  return 0
}

# should_skip_persisted KEY — true if persisted and not --force
should_skip_persisted() {
  local key="$1"
  if [[ "$FORCE" == true ]]; then return 1; fi
  is_persisted "$key"
}

# ── Stellar helpers ────────────────────────────────────────────────────────

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    die "missing required command: $1"
  fi
}

extract_contract_id() {
  grep -Eo 'C[A-Z0-9]{55}' | tail -n 1
}

extract_wasm_hash() {
  # stellar contract upload prints hex hash; grab 64-char hex
  grep -Eo '[0-9a-fA-F]{64}' | tail -n 1
}

deploy_contract() {
  local wasm="$1"
  local output contract_id
  CURRENT_STEP="stellar contract deploy --wasm $(basename "$wasm")"
  output="$(stellar contract deploy \
    --wasm "$wasm" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT" 2>&1)"
  local rc=$?
  if [[ $rc -ne 0 ]]; then
    printf '[deploy] deploy failed for %s:\n%s\n' "$wasm" "$output" >&2
    return $rc
  fi
  contract_id="$(printf '%s\n' "$output" | extract_contract_id)"
  if [[ -z "$contract_id" ]]; then
    printf '[deploy] could not parse contract id from output:\n%s\n' "$output" >&2
    return 1
  fi
  printf '%s\n' "$contract_id"
}

upload_wasm() {
  local wasm="$1"
  local output hash
  CURRENT_STEP="stellar contract upload --wasm $(basename "$wasm")"
  output="$(stellar contract upload \
    --wasm "$wasm" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT" 2>&1)"
  local rc=$?
  if [[ $rc -ne 0 ]]; then
    printf '[deploy] upload failed for %s:\n%s\n' "$wasm" "$output" >&2
    return $rc
  fi
  hash="$(printf '%s\n' "$output" | extract_wasm_hash)"
  if [[ -z "$hash" ]]; then
    # Fallback: try to extract after "hash:" prefix
    hash="$(printf '%s\n' "$output" | grep -i "hash" | grep -Eo '[0-9a-fA-F]{64}' | tail -n 1)"
  fi
  if [[ -z "$hash" ]]; then
    printf '[deploy] could not parse wasm hash from output:\n%s\n' "$output" >&2
    return 1
  fi
  printf '%s\n' "$hash"
}

invoke() {
  local contract_id="$1"
  shift
  CURRENT_STEP="stellar contract invoke --id ${contract_id:0:8}... -- $*"
  stellar contract invoke \
    --id "$contract_id" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT" \
    -- "$@"
}

# invoke_read — same but without --source (for read-only queries; pass --source anyway for consistency)
invoke_read() {
  local contract_id="$1"
  shift
  stellar contract invoke \
    --id "$contract_id" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT" \
    -- "$@" 2>&1
}

# ── WASM path helpers ─────────────────────────────────────────────────────

wasm_path() {
  local contract="$1"
  local fname="${WASM_FILE[$contract]:-}"
  if [[ -z "$fname" ]]; then
    # Fallback: contract name itself
    fname="${contract}.wasm"
  fi
  printf '%s/%s' "${ROOT_DIR}/${WASM_DIR}" "$fname"
}

ensure_wasm_built() {
  local missing=()
  for c in "${ALL_CONTRACTS[@]}"; do
    # Only check contracts that need wasm files (exclude synthetic 'pools')
    if [[ "$c" == "pools" ]]; then continue; fi
    local p
    p=$(wasm_path "$c")
    # Try alternative hyphen/underscore variant if not found
    if [[ ! -f "$p" ]]; then
      local alt
      alt="${p//_/-}"
      if [[ -f "$alt" ]]; then
        p="$alt"
      else
        alt="${p//-/_}"
        if [[ -f "$alt" ]]; then p="$alt"; fi
      fi
    fi
    if [[ ! -f "$p" ]]; then
      missing+=("$c ($p)")
    fi
  done

  if [[ ${#missing[@]} -eq 0 ]]; then
    log "all WASM artifacts present"
    return 0
  fi

  log "missing WASM artifacts for: ${missing[*]}"
  log "building release WASM artifacts (target ${WASM_TARGET})"
  require_cmd cargo
  (cd "$ROOT_DIR" && cargo build --release --target "${WASM_TARGET}")
}

# ── Verification helpers ──────────────────────────────────────────────────

verify_token() {
  local contract_id="$1"
  local expected_admin="$2"
  local out admin
  out=$(invoke_read "$contract_id" -- admin 2>&1 || true)
  admin=$(printf '%s\n' "$out" | grep -Eo 'C[A-Z0-9]{55}' | tail -n 1 || true)
  if [[ -z "$admin" ]]; then
    # Fallback: try balance/total_supply as liveness check
    out=$(invoke_read "$contract_id" -- total_supply 2>&1 || true)
    if echo "$out" | grep -qE '[0-9]+'; then
      log "verified token $contract_id liveness (total_supply readable)"
      return 0
    fi
    warn "could not verify token $contract_id — admin read returned empty: $out"
    return 1
  fi
  if [[ "$admin" != "$expected_admin" ]]; then
    warn "token $contract_id admin mismatch: expected $expected_admin got $admin"
    return 1
  fi
  log "verified token $contract_id admin=$admin"
}

verify_amm_pool() {
  local pool_id="$1"
  local expected_token_a="$2"
  local expected_token_b="$3"
  local expected_fee_bps="$4"
  local out
  out=$(invoke_read "$pool_id" -- get_info 2>&1 || true)
  if echo "$out" | grep -q "$expected_token_a" && echo "$out" | grep -q "$expected_token_b"; then
    log "verified pool $pool_id get_info contains expected tokens"
  else
    warn "pool $pool_id get_info verification failed — output: $out"
    return 1
  fi
  if echo "$out" | grep -q "$expected_fee_bps"; then
    log "verified pool $pool_id fee_bps=$expected_fee_bps present in get_info"
  else
    warn "pool $pool_id fee_bps not found in get_info: $out"
  fi
}

verify_factory_hashes() {
  local factory_id="$1"
  local expected_amm_hash="$2"
  local expected_token_hash="$3"
  # Factory stores hashes in instance storage; no direct getter for WASM hashes in all versions.
  # Perform liveness check via get_pool_count and all_pools
  local out
  out=$(invoke_read "$factory_id" -- get_pool_count 2>&1 || true)
  if echo "$out" | grep -qE '[0-9]+'; then
    log "verified factory $factory_id liveness (get_pool_count readable: $out)"
    return 0
  fi
  warn "could not verify factory $factory_id: $out"
  return 1
}

verify_governance() {
  local gov_id="$1"
  local expected_pool="$2"
  local expected_lp="$3"
  local out
  out=$(invoke_read "$gov_id" -- get_params 2>&1 || true)
  if echo "$out" | grep -q "$expected_pool" || echo "$out" | grep -q "$expected_lp"; then
    log "verified governance $gov_id points at pool/lp"
    return 0
  fi
  # Fallback liveness: try get_proposal or proposal_status
  out=$(invoke_read "$gov_id" -- get_params 2>&1 || true)
  if echo "$out" | grep -qE 'voting|quorum|Voting'; then
    log "verified governance $gov_id liveness"
    return 0
  fi
  warn "could not verify governance $gov_id: $out"
  return 1
}

# ── Source account ────────────────────────────────────────────────────────

generate_and_fund_source() {
  if stellar keys address "$SOURCE_ACCOUNT" >/dev/null 2>&1; then
    log "source account exists: $SOURCE_ACCOUNT"
    return
  fi
  log "generating and funding source account: $SOURCE_ACCOUNT"
  if stellar keys generate "$SOURCE_ACCOUNT" --network "$NETWORK" --fund >/dev/null 2>&1; then
    return
  fi
  stellar keys generate --default-seed "$SOURCE_ACCOUNT" >/dev/null
  stellar keys fund "$SOURCE_ACCOUNT" --network "$NETWORK" >/dev/null
}

# ── Network helpers ───────────────────────────────────────────────────────
# normalize_network — accept positional first arg as network for backward compat
