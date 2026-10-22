#!/usr/bin/env bash
# staking.sh — deploy and initialize Staking contract
# Sourceable module for deploy.sh

deploy_staking() {
  CURRENT_CONTRACT="staking"
  log "== staking =="

  local staking_wasm
  staking_wasm=$(wasm_path staking)

  if [[ -z "${LP_TOKEN_CONTRACT_ID:-}" ]]; then LP_TOKEN_CONTRACT_ID=$(get_persisted LP_TOKEN_CONTRACT_ID || echo ""); fi
  if [[ -z "${REWARD_TOKEN_CONTRACT_ID:-}" ]]; then REWARD_TOKEN_CONTRACT_ID=$(get_persisted REWARD_TOKEN_CONTRACT_ID || echo ""); fi

  if [[ -z "${LP_TOKEN_CONTRACT_ID:-}" || -z "${REWARD_TOKEN_CONTRACT_ID:-}" ]]; then
    warn "missing LP token or reward token — skipping staking"
    return 0
  fi

  if should_skip_persisted "STAKING_CONTRACT_ID"; then
    STAKING_CONTRACT_ID=$(get_persisted STAKING_CONTRACT_ID)
    log "skipping staking deploy (already at $STAKING_CONTRACT_ID)"
  else
    if ! should_deploy "staking"; then
      log "skipping staking deploy (--only/--skip filter)"
      STAKING_CONTRACT_ID=$(get_persisted STAKING_CONTRACT_ID || echo "")
      if [[ -z "$STAKING_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy staking"
      log "deploying staking: $staking_wasm"
      if [[ ! -f "$staking_wasm" ]]; then
        warn "staking WASM not found: $staking_wasm — skipping"
        return 0
      fi
      STAKING_CONTRACT_ID=$(deploy_contract "$staking_wasm")
      persist_var "STAKING_CONTRACT_ID" "$STAKING_CONTRACT_ID"
      log "staking: $STAKING_CONTRACT_ID"
    fi
  fi

  export STAKING_CONTRACT_ID

  if [[ -z "${STAKING_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "STAKING_INITIALIZED"; then
    log "skipping staking initialize (already done)"
  else
    if ! should_deploy "staking"; then
      log "skipping staking initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize staking"
      log "initializing staking: lp=$LP_TOKEN_CONTRACT_ID reward=$REWARD_TOKEN_CONTRACT_ID admin=$ADMIN_ADDRESS"
      if ! invoke "$STAKING_CONTRACT_ID" initialize \
          --lp_token "$LP_TOKEN_CONTRACT_ID" \
          --reward_token "$REWARD_TOKEN_CONTRACT_ID" \
          --admin "$ADMIN_ADDRESS" >/dev/null 2>&1; then
        if invoke_read "$STAKING_CONTRACT_ID" get_pool_info >/dev/null 2>/dev/null; then
          log "staking already initialized"
        else
          die "failed to initialize staking"
        fi
      else
        log "staking initialized"
      fi
      persist_var "STAKING_INITIALIZED" "1"
    fi
  fi

  CURRENT_STEP="verify staking"
  verify_staking "$STAKING_CONTRACT_ID" "$LP_TOKEN_CONTRACT_ID" "$REWARD_TOKEN_CONTRACT_ID" "$ADMIN_ADDRESS"
}

# verify_staking STAKING_ID EXPECTED_LP EXPECTED_REWARD EXPECTED_ADMIN
# staking `initialize` takes no require_auth, so whoever calls it first picks
# the admin and the tokens. Read the stored values back (#1114) and abort the
# deploy on any difference instead of carrying on with a hijacked instance.
verify_staking() {
  local staking_id="$1"
  local expected_lp="$2"
  local expected_reward="$3"
  local expected_admin="$4"
  verify_contract_admin staking "$staking_id" "$expected_admin" get_admin
  local cfg
  cfg=$(read_back staking "$staking_id" get_config) \
    || die "staking $staking_id get_config returned nothing after retries — contract is not initialized"
  grep -q "$expected_lp" <<<"$cfg" \
    || die "staking $staking_id lp_token mismatch: expected $expected_lp not found in get_config: $cfg"
  grep -q "$expected_reward" <<<"$cfg" \
    || die "staking $staking_id reward_token mismatch: expected $expected_reward not found in get_config: $cfg"
  log "verified staking $staking_id config (lp_token, reward_token, admin)"
}
