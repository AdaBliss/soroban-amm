#!/usr/bin/env bash
# twal_consumer.sh — deploy the TWAL (time-weighted average liquidity) Consumer
# Sourceable module for deploy.sh
#
# Depends on: pools (it reads liquidity from them). After initializing, the
# pools deployed by pools.sh are registered as tracked so keepers can start
# snapshotting them; registration needs the keeper's signature, so it only
# happens when the keeper is the deploying account.

deploy_twal_consumer() {
  CURRENT_CONTRACT="twal_consumer"
  log "== twal_consumer =="

  local wasm
  wasm=$(wasm_path twal_consumer)

  if should_skip_persisted "TWAL_CONSUMER_CONTRACT_ID"; then
    TWAL_CONSUMER_CONTRACT_ID=$(get_persisted TWAL_CONSUMER_CONTRACT_ID)
    log "skipping twal_consumer deploy (already at $TWAL_CONSUMER_CONTRACT_ID)"
  else
    if ! should_deploy "twal_consumer"; then
      log "skipping twal_consumer deploy (--only/--skip filter)"
      TWAL_CONSUMER_CONTRACT_ID=$(get_persisted TWAL_CONSUMER_CONTRACT_ID || echo "")
      if [[ -z "$TWAL_CONSUMER_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy twal_consumer"
      log "deploying twal_consumer: $wasm"
      if [[ ! -f "$wasm" ]]; then
        warn "WASM not found: $wasm — skipping"
        return 0
      fi
      TWAL_CONSUMER_CONTRACT_ID=$(deploy_contract "$wasm")
      persist_var "TWAL_CONSUMER_CONTRACT_ID" "$TWAL_CONSUMER_CONTRACT_ID"
      log "twal_consumer: $TWAL_CONSUMER_CONTRACT_ID"
    fi
  fi

  export TWAL_CONSUMER_CONTRACT_ID
  if [[ -z "${TWAL_CONSUMER_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "TWAL_CONSUMER_INITIALIZED"; then
    log "skipping twal_consumer initialize (already done)"
  else
    if ! should_deploy "twal_consumer"; then
      log "skipping twal_consumer initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize twal_consumer"
      log "initializing twal_consumer keeper=$ADMIN_ADDRESS"
      if invoke "$TWAL_CONSUMER_CONTRACT_ID" initialize --keeper "$ADMIN_ADDRESS" >/dev/null 2>&1; then
        log "twal_consumer initialized"
      elif invoke_read "$TWAL_CONSUMER_CONTRACT_ID" get_keeper | grep -q "$ADMIN_ADDRESS"; then
        # A previous run initialized it but died before persisting the marker.
        log "twal_consumer already initialized"
      else
        die "failed to initialize twal_consumer at $TWAL_CONSUMER_CONTRACT_ID"
      fi
      persist_var "TWAL_CONSUMER_INITIALIZED" "1"
    fi
  fi

  if should_deploy "twal_consumer"; then
    twal_consumer_track_pool "${AMM_POOL_CONTRACT_ID:-}" Amm
    twal_consumer_track_pool "${CL_POOL_CONTRACT_ID:-}" Cl
  fi

  CURRENT_STEP="verify twal_consumer"
  local keeper
  keeper=$(invoke_read "$TWAL_CONSUMER_CONTRACT_ID" get_keeper || true)
  if printf '%s\n' "$keeper" | grep -q "$ADMIN_ADDRESS"; then
    log "verified twal_consumer keeper=$ADMIN_ADDRESS"
  else
    die "twal_consumer $TWAL_CONSUMER_CONTRACT_ID keeper mismatch: expected $ADMIN_ADDRESS, got: $keeper"
  fi
  log "twal_consumer tracks $(invoke_read "$TWAL_CONSUMER_CONTRACT_ID" get_tracked_pool_count | tail -n 1) pool(s)"
}

# twal_consumer_track_pool POOL_ID POOL_TYPE — register a deployed pool with
# the consumer (idempotent) and read it back.
twal_consumer_track_pool() {
  local pool="$1"
  local pool_type="$2"
  if [[ -z "$pool" ]]; then return 0; fi

  if invoke_read "$TWAL_CONSUMER_CONTRACT_ID" is_tracked --pool "$pool" | grep -qx 'true'; then
    log "twal_consumer already tracks $pool_type pool $pool"
    return 0
  fi
  if [[ "$ADMIN_ADDRESS" != "$SOURCE_PUBLIC_KEY" ]]; then
    warn "twal_consumer keeper $ADMIN_ADDRESS is not the deploying account; register the $pool_type pool as the keeper:"
    warn "  stellar contract invoke --id $TWAL_CONSUMER_CONTRACT_ID --network $NETWORK --source <keeper> -- add_tracked_pool --pool $pool --pool_type '\"$pool_type\"'"
    return 0
  fi

  CURRENT_STEP="twal_consumer add_tracked_pool $pool_type"
  invoke "$TWAL_CONSUMER_CONTRACT_ID" add_tracked_pool --pool "$pool" --pool_type "\"$pool_type\"" >/dev/null 2>&1 \
    || die "failed to register $pool_type pool $pool with twal_consumer"
  if invoke_read "$TWAL_CONSUMER_CONTRACT_ID" is_tracked --pool "$pool" | grep -qx 'true'; then
    log "verified twal_consumer tracks $pool_type pool $pool"
  else
    die "twal_consumer does not report $pool_type pool $pool as tracked after add_tracked_pool"
  fi
}
