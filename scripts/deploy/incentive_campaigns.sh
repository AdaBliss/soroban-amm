#!/usr/bin/env bash
# incentive_campaigns.sh — deploy Incentive Campaigns
# Sourceable module for deploy.sh

deploy_incentive_campaigns() {
  CURRENT_CONTRACT="incentive_campaigns"
  log "== incentive_campaigns =="

  local wasm
  wasm=$(wasm_path incentive_campaigns)

  if [[ -z "${GOVERNANCE_CONTRACT_ID:-}" ]]; then GOVERNANCE_CONTRACT_ID=$(get_persisted GOVERNANCE_CONTRACT_ID || echo ""); fi
  if [[ -z "${GOVERNANCE_CONTRACT_ID:-}" ]]; then
    warn "governance not deployed — skipping incentive_campaigns (requires governance)"
    return 0
  fi

  if should_skip_persisted "INCENTIVE_CAMPAIGNS_CONTRACT_ID"; then
    INCENTIVE_CAMPAIGNS_CONTRACT_ID=$(get_persisted INCENTIVE_CAMPAIGNS_CONTRACT_ID)
    log "skipping incentive_campaigns deploy (already at $INCENTIVE_CAMPAIGNS_CONTRACT_ID)"
  else
    if ! should_deploy "incentive_campaigns"; then
      log "skipping incentive_campaigns deploy (--only/--skip filter)"
      INCENTIVE_CAMPAIGNS_CONTRACT_ID=$(get_persisted INCENTIVE_CAMPAIGNS_CONTRACT_ID || echo "")
      if [[ -z "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy incentive_campaigns"
      log "deploying incentive_campaigns: $wasm"
      if [[ ! -f "$wasm" ]]; then
        warn "WASM not found: $wasm — skipping"
        return 0
      fi
      INCENTIVE_CAMPAIGNS_CONTRACT_ID=$(deploy_contract "$wasm")
      persist_var "INCENTIVE_CAMPAIGNS_CONTRACT_ID" "$INCENTIVE_CAMPAIGNS_CONTRACT_ID"
      log "incentive_campaigns: $INCENTIVE_CAMPAIGNS_CONTRACT_ID"
    fi
  fi

  export INCENTIVE_CAMPAIGNS_CONTRACT_ID
  if [[ -z "${INCENTIVE_CAMPAIGNS_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "INCENTIVE_CAMPAIGNS_INITIALIZED"; then
    log "skipping incentive_campaigns initialize (already done)"
  else
    if ! should_deploy "incentive_campaigns"; then
      log "skipping incentive_campaigns initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize incentive_campaigns"
      log "initializing incentive_campaigns governance=$GOVERNANCE_CONTRACT_ID"
      if ! invoke "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" initialize --governance "$GOVERNANCE_CONTRACT_ID" >/dev/null 2>&1; then
        warn "failed to initialize incentive_campaigns — may already be initialized"
        # Verify liveness as fallback
        if invoke_read "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" list_campaigns >/dev/null 2>/dev/null || invoke_read "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" get_next_campaign_id >/dev/null 2>/dev/null; then
          log "incentive_campaigns already initialized (liveness ok)"
        fi
      else
        log "incentive_campaigns initialized"
      fi
      persist_var "INCENTIVE_CAMPAIGNS_INITIALIZED" "1"
    fi
  fi

  CURRENT_STEP="verify incentive_campaigns"
  local ver paused
  ver=$(invoke_read "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" version 2>/dev/null || echo "")
  paused=$(invoke_read "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" is_paused 2>/dev/null || echo "")
  if (invoke_read "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" get_next_campaign_id >/dev/null 2>/dev/null || invoke_read "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" list_campaigns >/dev/null 2>/dev/null) && [[ -n "$ver" ]] && [[ -n "$paused" ]]; then
    log "verified incentive_campaigns liveness (version=$ver is_paused=$paused)"
  elif invoke_read "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" get_next_campaign_id >/dev/null 2>/dev/null || invoke_read "$INCENTIVE_CAMPAIGNS_CONTRACT_ID" list_campaigns >/dev/null 2>/dev/null; then
    log "verified incentive_campaigns liveness"
  else
    warn "incentive_campaigns verification: read failed"
  fi
}
