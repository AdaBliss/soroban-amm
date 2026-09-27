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
      log "initializing batch_router factory=$FACTORY_CONTRACT_ID"
      if invoke "$BATCH_ROUTER_CONTRACT_ID" initialize --factory "$FACTORY_CONTRACT_ID" >/dev/null 2>&1; then
        log "batch_router initialized"
      elif batch_router_is_initialized "$BATCH_ROUTER_CONTRACT_ID"; then
        # A previous run initialized it but died before persisting the marker.
        log "batch_router already initialized"
      else
        die "failed to initialize batch_router at $BATCH_ROUTER_CONTRACT_ID"
      fi
      persist_var "BATCH_ROUTER_INITIALIZED" "1"
    fi
  fi

  # batch_router exposes no factory getter. simulate_batch loads the stored
  # factory unconditionally, so an empty batch returns `[]` once initialize
  # has run and traps before it has — a read-only probe of the init state.
  CURRENT_STEP="verify batch_router"
  if batch_router_is_initialized "$BATCH_ROUTER_CONTRACT_ID"; then
    log "verified batch_router $BATCH_ROUTER_CONTRACT_ID is initialized (simulate_batch readable)"
  else
    die "batch_router $BATCH_ROUTER_CONTRACT_ID verification failed: simulate_batch did not return an empty result"
  fi
}

# batch_router_is_initialized ID — true when simulate_batch([]) succeeds.
batch_router_is_initialized() {
  local out
  out=$(invoke_read "$1" simulate_batch --ops '[]' || true)
  printf '%s\n' "$out" | grep -qE '^\[\]$'
}
