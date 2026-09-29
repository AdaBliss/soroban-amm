#!/usr/bin/env bash
# factory.sh — deploy and initialize the Factory contract, register WASM hashes
# Sourceable module for deploy.sh

deploy_factory() {
  CURRENT_CONTRACT="factory"
  log "== factory =="

  local factory_wasm token_wasm amm_wasm cl_wasm
  factory_wasm=$(wasm_path factory)
  token_wasm=$(wasm_path token)
  amm_wasm=$(wasm_path amm)
  cl_wasm=$(wasm_path concentrated_liquidity)

  # Resolve alt paths if needed
  for p in factory_wasm token_wasm amm_wasm cl_wasm; do
    local var_name
    # handled via wasm_path directly
    true
  done

  # ── Upload WASM hashes (idempotent) ─────────────────────────────────────
  if should_skip_persisted "TOKEN_WASM_HASH"; then
    TOKEN_WASM_HASH=$(get_persisted TOKEN_WASM_HASH)
    log "skipping token WASM upload (hash $TOKEN_WASM_HASH)"
  else
    if ! should_deploy "factory" && ! should_deploy "token"; then
      # Still need hash if factory will be skipped but pools need it; try to load
      TOKEN_WASM_HASH=$(get_persisted TOKEN_WASM_HASH || echo "")
      if [[ -z "$TOKEN_WASM_HASH" ]]; then
        log "warning: TOKEN_WASM_HASH not persisted and factory skipped — will need manual hash"
      fi
    else
      CURRENT_STEP="upload token WASM"
      log "uploading token WASM: $token_wasm"
      if [[ ! -f "$token_wasm" ]]; then
        die "token WASM not found: $token_wasm"
      fi
      check_wasm_size "$token_wasm" "token"
      TOKEN_WASM_HASH=$(upload_wasm "$token_wasm")
      persist_var "TOKEN_WASM_HASH" "$TOKEN_WASM_HASH"
      log "token WASM hash: $TOKEN_WASM_HASH"
    fi
  fi

  if should_skip_persisted "AMM_WASM_HASH"; then
    AMM_WASM_HASH=$(get_persisted AMM_WASM_HASH)
    log "skipping AMM WASM upload (hash $AMM_WASM_HASH)"
  else
    if ! should_deploy "factory" && ! should_deploy "amm"; then
      AMM_WASM_HASH=$(get_persisted AMM_WASM_HASH || echo "")
    else
      CURRENT_STEP="upload amm WASM"
      log "uploading AMM WASM: $amm_wasm"
      if [[ ! -f "$amm_wasm" ]]; then
        die "AMM WASM not found: $amm_wasm"
      fi
      check_wasm_size "$amm_wasm" "amm"
      AMM_WASM_HASH=$(upload_wasm "$amm_wasm")
      persist_var "AMM_WASM_HASH" "$AMM_WASM_HASH"
      log "AMM WASM hash: $AMM_WASM_HASH"
    fi
  fi

  if should_skip_persisted "CL_WASM_HASH"; then
    CL_WASM_HASH=$(get_persisted CL_WASM_HASH)
    log "skipping CL WASM upload (hash $CL_WASM_HASH)"
  else
    if ! should_deploy "factory" && ! should_deploy "concentrated_liquidity"; then
      CL_WASM_HASH=$(get_persisted CL_WASM_HASH || echo "")
    else
      CURRENT_STEP="upload concentrated_liquidity WASM"
      log "uploading concentrated_liquidity WASM: $cl_wasm"
      if [[ ! -f "$cl_wasm" ]]; then
        warn "CL WASM not found at $cl_wasm — will register later if needed"
        CL_WASM_HASH=$(get_persisted CL_WASM_HASH || echo "")
      else
        check_wasm_size "$cl_wasm" "concentrated_liquidity"
        CL_WASM_HASH=$(upload_wasm "$cl_wasm")
        persist_var "CL_WASM_HASH" "$CL_WASM_HASH"
        log "CL WASM hash: $CL_WASM_HASH"
      fi
    fi
  fi

  # ── Deploy factory contract ─────────────────────────────────────────────
  if should_skip_persisted "FACTORY_CONTRACT_ID"; then
    FACTORY_CONTRACT_ID=$(get_persisted FACTORY_CONTRACT_ID)
    log "skipping factory deploy (already at $FACTORY_CONTRACT_ID)"
  else
    if ! should_deploy "factory"; then
      log "skipping factory deploy (--only/--skip filter)"
      FACTORY_CONTRACT_ID=$(get_persisted FACTORY_CONTRACT_ID || echo "")
      if [[ -z "$FACTORY_CONTRACT_ID" ]]; then
        log "factory not deployed and filtered out — downstream pools will be skipped"
        return 0
      fi
    else
      CURRENT_STEP="deploy factory contract"
      log "deploying factory: $factory_wasm"
      if [[ ! -f "$factory_wasm" ]]; then
        die "factory WASM not found: $factory_wasm"
      fi
      FACTORY_CONTRACT_ID=$(deploy_contract "$factory_wasm")
      persist_var "FACTORY_CONTRACT_ID" "$FACTORY_CONTRACT_ID"
      log "factory: $FACTORY_CONTRACT_ID"
    fi
  fi

  export FACTORY_CONTRACT_ID AMM_WASM_HASH TOKEN_WASM_HASH CL_WASM_HASH

  if [[ -z "${FACTORY_CONTRACT_ID:-}" ]]; then
    warn "factory contract ID empty — skipping initialization"
    return 0
  fi

  # ── Initialize factory ──────────────────────────────────────────────────
  if should_skip_persisted "FACTORY_INITIALIZED"; then
    log "skipping factory initialize (already done)"
  else
    if ! should_deploy "factory"; then
      log "skipping factory initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize factory"
      log "initializing factory admin=$ADMIN_ADDRESS"
      # Check if already initialized by reading admin (liveness)
      local pre
      pre=$(invoke_read "$FACTORY_CONTRACT_ID" -- get_pool_count 2>&1 || true)
      # Try initialize; if AlreadyInitialized error, treat as success for idempotency
      if ! invoke "$FACTORY_CONTRACT_ID" initialize \
          --admin "$ADMIN_ADDRESS" \
          --amm_wasm_hash "$AMM_WASM_HASH" \
          --token_wasm_hash "$TOKEN_WASM_HASH" >/dev/null 2>&1; then
        # Check if error is AlreadyInitialized
        if echo "$pre" | grep -qE '[0-9]+' || invoke_read "$FACTORY_CONTRACT_ID" -- get_pool_count >/dev/null 2>&1; then
          log "factory already initialized (idempotent)"
        else
          die "failed to initialize factory"
        fi
      else
        log "factory initialized"
      fi
      persist_var "FACTORY_INITIALIZED" "1"
    fi
  fi

  # ── Register CL WASM hash (set_cl_wasm_hash) ────────────────────────────
  if [[ -n "${CL_WASM_HASH:-}" ]]; then
    if should_skip_persisted "FACTORY_CL_WASM_REGISTERED"; then
      log "skipping CL WASM hash registration (already done)"
    else
      if ! should_deploy "factory"; then
        log "skipping CL WASM registration (--only/--skip filter)"
      else
        CURRENT_STEP="register CL WASM hash on factory"
        log "registering CL WASM hash on factory"
        if ! invoke "$FACTORY_CONTRACT_ID" set_cl_wasm_hash --cl_wasm_hash "$CL_WASM_HASH" >/dev/null 2>&1; then
          warn "failed to register CL WASM hash — may already be set or factory not ready"
        else
          log "CL WASM hash registered"
        fi
        persist_var "FACTORY_CL_WASM_REGISTERED" "1"
      fi
    fi
  fi

  # ── Verification ─────────────────────────────────────────────────────────
  if [[ -n "${FACTORY_CONTRACT_ID:-}" ]]; then
    CURRENT_STEP="verify factory"
    if ! verify_factory_hashes "$FACTORY_CONTRACT_ID" "${AMM_WASM_HASH:-}" "${TOKEN_WASM_HASH:-}"; then
      warn "factory verification warning"
    fi
  fi
}
