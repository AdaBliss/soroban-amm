#!/usr/bin/env bash
# batch_router.sh — deploy the Batch Router
# Sourceable module for deploy.sh
#
# Depends on: factory (resolves pools for every op). Runs after batch_auction
# so the auction and the router that fronts it are deployed together.

deploy_batch_router() {
  CURRENT_CONTRACT="batch_router"
  log "== batch_router =="

  local wasm
  wasm=$(wasm_path batch_router)

  if [[ -z "${FACTORY_CONTRACT_ID:-}" ]]; then FACTORY_CONTRACT_ID=$(get_persisted FACTORY_CONTRACT_ID || echo ""); fi
  if [[ -z "${FACTORY_CONTRACT_ID:-}" ]]; then
    warn "factory not deployed — skipping batch_router"
    return 0
  fi

  if should_skip_persisted "BATCH_ROUTER_CONTRACT_ID"; then
    BATCH_ROUTER_CONTRACT_ID=$(get_persisted BATCH_ROUTER_CONTRACT_ID)
    log "skipping batch_router deploy (already at $BATCH_ROUTER_CONTRACT_ID)"
  else
    if ! should_deploy "batch_router"; then
      log "skipping batch_router deploy (--only/--skip filter)"
      BATCH_ROUTER_CONTRACT_ID=$(get_persisted BATCH_ROUTER_CONTRACT_ID || echo "")
      if [[ -z "$BATCH_ROUTER_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy batch_router"
      log "deploying batch_router: $wasm"
      if [[ ! -f "$wasm" ]]; then
        warn "WASM not found: $wasm — skipping"
        return 0
      fi
      BATCH_ROUTER_CONTRACT_ID=$(deploy_contract "$wasm")
      persist_var "BATCH_ROUTER_CONTRACT_ID" "$BATCH_ROUTER_CONTRACT_ID"
      log "batch_router: $BATCH_ROUTER_CONTRACT_ID"
    fi
  fi

  export BATCH_ROUTER_CONTRACT_ID
  if [[ -z "${BATCH_ROUTER_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "BATCH_ROUTER_INITIALIZED"; then
    log "skipping batch_router initialize (already done)"
  else
    if ! should_deploy "batch_router"; then
      log "skipping batch_router initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize batch_router"
      log "initializing batch_router admin=$ADMIN_ADDRESS factory=$FACTORY_CONTRACT_ID"
      if invoke "$BATCH_ROUTER_CONTRACT_ID" initialize --admin "$ADMIN_ADDRESS" --factory "$FACTORY_CONTRACT_ID" >/dev/null 2>&1; then
        log "batch_router initialized"
      elif invoke_read "$BATCH_ROUTER_CONTRACT_ID" get_factory >/dev/null 2>&1; then
        # A previous run initialized it but died before persisting the marker.
        # The verification below decides whether its values are the intended ones.
        log "batch_router already initialized"
      else
        die "failed to initialize batch_router at $BATCH_ROUTER_CONTRACT_ID"
      fi
      persist_var "BATCH_ROUTER_INITIALIZED" "1"
    fi
  fi

  CURRENT_STEP="verify batch_router"
  verify_batch_router "$BATCH_ROUTER_CONTRACT_ID" "$ADMIN_ADDRESS" "$FACTORY_CONTRACT_ID"
}

# verify_batch_router ID EXPECTED_ADMIN EXPECTED_FACTORY
# batch_router `initialize` takes no require_auth (an attacker would just
# authorize their own address), so whoever calls it first picks the admin and
# the factory. Read both back and abort the deploy on any difference instead of
# carrying on with a hijacked instance.
verify_batch_router() {
  local router_id="$1"
  local expected_admin="$2"
  local expected_factory="$3"
  verify_contract_admin batch_router "$router_id" "$expected_admin" get_admin
  local out factory
  out=$(read_back batch_router "$router_id" get_factory) \
    || die "batch_router $router_id get_factory returned nothing after retries — contract is not initialized"
  factory=$(printf '%s\n' "$out" | extract_contract_id || true)
  if [[ -z "$factory" ]]; then
    die "batch_router $router_id get_factory returned no address — contract is not initialized"
  fi
  if [[ "$factory" != "$expected_factory" ]]; then
    die "batch_router $router_id factory mismatch: expected $expected_factory got $factory — possible front-run, aborting deploy"
  fi
  log "verified batch_router $router_id config (admin, factory)"
}
