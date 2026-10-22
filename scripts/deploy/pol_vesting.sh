#!/usr/bin/env bash
# pol_vesting.sh — deploy POL Vesting
# Sourceable module for deploy.sh

deploy_pol_vesting() {
  CURRENT_CONTRACT="pol_vesting"
  log "== pol_vesting =="

  local wasm
  wasm=$(wasm_path pol_vesting)

  if [[ -z "${GOVERNANCE_CONTRACT_ID:-}" ]]; then GOVERNANCE_CONTRACT_ID=$(get_persisted GOVERNANCE_CONTRACT_ID || echo ""); fi
  if [[ -z "${GOVERNANCE_CONTRACT_ID:-}" ]]; then
    warn "governance not deployed — skipping pol_vesting"
    return 0
  fi

  if should_skip_persisted "POL_VESTING_CONTRACT_ID"; then
    POL_VESTING_CONTRACT_ID=$(get_persisted POL_VESTING_CONTRACT_ID)
    log "skipping pol_vesting deploy (already at $POL_VESTING_CONTRACT_ID)"
  else
    if ! should_deploy "pol_vesting"; then
      log "skipping pol_vesting deploy (--only/--skip filter)"
      POL_VESTING_CONTRACT_ID=$(get_persisted POL_VESTING_CONTRACT_ID || echo "")
      if [[ -z "$POL_VESTING_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy pol_vesting"
      log "deploying pol_vesting: $wasm"
      if [[ ! -f "$wasm" ]]; then
        warn "WASM not found: $wasm — skipping"
        return 0
      fi
      POL_VESTING_CONTRACT_ID=$(deploy_contract "$wasm")
      persist_var "POL_VESTING_CONTRACT_ID" "$POL_VESTING_CONTRACT_ID"
      log "pol_vesting: $POL_VESTING_CONTRACT_ID"
    fi
  fi

  export POL_VESTING_CONTRACT_ID
  if [[ -z "${POL_VESTING_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "POL_VESTING_INITIALIZED"; then
    log "skipping pol_vesting initialize (already done)"
  else
    if ! should_deploy "pol_vesting"; then
      log "skipping pol_vesting initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize pol_vesting"
      log "initializing pol_vesting governance=$GOVERNANCE_CONTRACT_ID treasury=$ADMIN_ADDRESS"
      if ! invoke "$POL_VESTING_CONTRACT_ID" initialize --governance "$GOVERNANCE_CONTRACT_ID" --treasury "$ADMIN_ADDRESS" >/dev/null 2>&1; then
        if invoke_read "$POL_VESTING_CONTRACT_ID" get_governance 2>/dev/null | grep -q "$GOVERNANCE_CONTRACT_ID"; then
          log "pol_vesting already initialized"
        else
          warn "failed to initialize pol_vesting"
        fi
      else
        log "pol_vesting initialized"
      fi
      persist_var "POL_VESTING_INITIALIZED" "1"
    fi
  fi

  # Runs whether this run initialized the contract or found it initialized:
  # initialize takes no auth, so whoever called it first chose governance and
  # treasury. Abort rather than keep a contract configured by someone else (#1136).
  CURRENT_STEP="verify pol_vesting configuration"
  verify_pol_vesting "$POL_VESTING_CONTRACT_ID" "$GOVERNANCE_CONTRACT_ID" "$ADMIN_ADDRESS"
}

# verify_pol_vesting ID EXPECTED_GOVERNANCE EXPECTED_TREASURY
# Reads the stored governance and treasury back with read_back and compares
# each exact address. A contract that does not answer, or that stores a
# different address, aborts the deploy.
verify_pol_vesting() {
  local id="$1"
  local expected_governance="$2"
  local expected_treasury="$3"
  local getter out stored
  for getter in get_governance get_treasury; do
    out=$(read_back pol_vesting "$id" "$getter") \
      || die "pol_vesting $id $getter returned nothing after retries — contract is not initialized"
    stored=$(printf '%s\n' "$out" | grep -Eo '[GC][A-Z2-7]{55}' | tail -n 1 || true)
    if [[ -z "$stored" ]]; then
      die "pol_vesting $id $getter returned no address — possible front-run or uninitialized contract"
    fi
    local expected="$expected_governance"
    if [[ "$getter" == "get_treasury" ]]; then expected="$expected_treasury"; fi
    if [[ "$stored" != "$expected" ]]; then
      die "pol_vesting $id $getter mismatch: expected $expected got $stored — possible front-run, aborting deploy"
    fi
  done
  log "verified pol_vesting $id governance and treasury"
}
