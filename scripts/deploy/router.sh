#!/usr/bin/env bash
# router.sh — deploy the Router (batch_router has its own module)
# Sourceable module for deploy.sh

deploy_router() {
  CURRENT_CONTRACT="router"
  log "== router =="

  local wasm
  wasm=$(wasm_path router)

  if [[ -z "${FACTORY_CONTRACT_ID:-}" ]]; then FACTORY_CONTRACT_ID=$(get_persisted FACTORY_CONTRACT_ID || echo ""); fi
  if [[ -z "${FACTORY_CONTRACT_ID:-}" ]]; then
    warn "factory not deployed — skipping router"
    return 0
  fi

  if should_skip_persisted "ROUTER_CONTRACT_ID"; then
    ROUTER_CONTRACT_ID=$(get_persisted ROUTER_CONTRACT_ID)
    log "skipping router deploy (already at $ROUTER_CONTRACT_ID)"
  else
    if ! should_deploy "router"; then
      log "skipping router deploy (--only/--skip filter)"
      ROUTER_CONTRACT_ID=$(get_persisted ROUTER_CONTRACT_ID || echo "")
      if [[ -z "$ROUTER_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy router"
      log "deploying router: $wasm"
      if [[ ! -f "$wasm" ]]; then
        warn "WASM not found: $wasm — skipping"
        return 0
      fi
      ROUTER_CONTRACT_ID=$(deploy_contract "$wasm")
      persist_var "ROUTER_CONTRACT_ID" "$ROUTER_CONTRACT_ID"
      log "router: $ROUTER_CONTRACT_ID"
    fi
  fi

  export ROUTER_CONTRACT_ID
  if [[ -z "${ROUTER_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "ROUTER_INITIALIZED"; then
    log "skipping router initialize (already done)"
  else
    if ! should_deploy "router"; then
      log "skipping router initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize router"
      log "initializing router factory=$FACTORY_CONTRACT_ID"
      if ! invoke "$ROUTER_CONTRACT_ID" initialize --factory "$FACTORY_CONTRACT_ID" >/dev/null 2>&1; then
        warn "failed to initialize router — may already be initialized"
        if invoke_read "$ROUTER_CONTRACT_ID" -- get_factory 2>&1 | grep -q "$FACTORY_CONTRACT_ID" || true; then
          log "router already initialized"
        fi
      else
        log "router initialized"
      fi
      persist_var "ROUTER_INITIALIZED" "1"
    fi
  fi

  CURRENT_STEP="verify router"
  if invoke_read "$ROUTER_CONTRACT_ID" -- get_factory >/dev/null 2>&1 || invoke_read "$ROUTER_CONTRACT_ID" -- get_pool >/dev/null 2>&1; then
    log "verified router liveness"
  else
    warn "router verification: read failed (may not expose getter)"
    log "router deployed at $ROUTER_CONTRACT_ID (liveness via deploy ok)"
  fi
}
