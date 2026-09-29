#!/usr/bin/env bash
# preflight.sh — verify the network and the source identity before the e2e
# suite runs, so infrastructure problems fail loudly as infrastructure instead
# of surfacing later as a failed deploy that looks like a protocol regression.
#
# Checks, in order:
#   1. the RPC endpoint reports itself healthy;
#   2. the source account exists on the network. A missing account means the
#      network was reset (testnet and futurenet are wiped periodically) or the
#      key was never funded; where the network has a friendbot, the account is
#      re-created through it, and a friendbot rate limit is reported as such;
#   3. the account holds at least SMOKE_MIN_BALANCE_XLM (default 500), enough
#      for a full deployment. Friendbot only funds accounts that do not exist
#      yet, so a drained account has to be topped up or the key rotated.
#
# Usage:
#   SOURCE_ACCOUNT=ci-smoke bash scripts/e2e/preflight.sh
#
# Environment:
#   STELLAR_NETWORK / NETWORK   testnet (default), futurenet or mainnet
#   SOURCE_ACCOUNT              stellar CLI identity the suite signs with
#   SMOKE_RPC_URL               override the RPC endpoint for the network
#   SMOKE_HORIZON_URL           override the Horizon endpoint for the network
#   SMOKE_MIN_BALANCE_XLM       minimum native balance to proceed (default 500)
#
# Exit status: 0 when the suite can run; 75 (EX_TEMPFAIL) for an
# infrastructure problem. Under GitHub Actions the reason is also written to
# $GITHUB_OUTPUT as `reason` and raised as an error annotation.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# shellcheck source=scripts/e2e/common.sh
source "$ROOT_DIR/e2e/common.sh"

readonly EX_INFRA=75
MIN_BALANCE_XLM="${SMOKE_MIN_BALANCE_XLM:-500}"

infra_fail() {
  # One line: $GITHUB_OUTPUT is key=value per line, and curl retries repeat
  # their error once per attempt.
  local reason
  reason="$(printf '%s' "$1" | tr -s '\r\n' '  ' | cut -c1-400)"
  printf '[INFRA] %s\n' "$reason" >&2
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    printf 'reason=%s\n' "$reason" >>"$GITHUB_OUTPUT"
    printf '::error title=Smoke test infrastructure failure::%s\n' "$reason"
  fi
  exit "$EX_INFRA"
}

case "$NETWORK" in
  testnet)
    RPC_URL="${SMOKE_RPC_URL:-https://soroban-testnet.stellar.org}"
    HORIZON_URL="${SMOKE_HORIZON_URL:-https://horizon-testnet.stellar.org}"
    ;;
  futurenet)
    RPC_URL="${SMOKE_RPC_URL:-https://rpc-futurenet.stellar.org}"
    HORIZON_URL="${SMOKE_HORIZON_URL:-https://horizon-futurenet.stellar.org}"
    ;;
  *)
    RPC_URL="${SMOKE_RPC_URL:?SMOKE_RPC_URL must be set for network $NETWORK}"
    HORIZON_URL="${SMOKE_HORIZON_URL:?SMOKE_HORIZON_URL must be set for network $NETWORK}"
    ;;
esac

require_cmd curl
require_cmd jq
require_cmd stellar

rpc() {
  curl -sS --fail --max-time 30 --retry 3 --retry-all-errors \
    -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\"}" \
    "$RPC_URL"
}

# ── 1. RPC health ──────────────────────────────────────────────────────────
health="$(rpc getHealth 2>&1)" || infra_fail "RPC $RPC_URL is unreachable: $health"
status="$(jq -r '.result.status // .error.message // "unknown"' <<<"$health")"
[[ "$status" == "healthy" ]] || infra_fail "RPC $RPC_URL reports status '$status'"
latest_ledger="$(jq -r '.result.latestLedger' <<<"$health")"
pass "RPC healthy: $RPC_URL (latest ledger $latest_ledger)"

network_info="$(rpc getNetwork 2>&1)" || infra_fail "RPC getNetwork failed: $network_info"
friendbot_url="$(jq -r '.result.friendbotUrl // empty' <<<"$network_info")"
pass "network: $(jq -r '.result.passphrase' <<<"$network_info") (protocol $(jq -r '.result.protocolVersion' <<<"$network_info"))"

# ── 2. Source account exists ───────────────────────────────────────────────
address="$(stellar keys address "$SOURCE_ACCOUNT" 2>&1)" \
  || infra_fail "identity '$SOURCE_ACCOUNT' is not configured: $address"

account_body="$(mktemp)"
trap 'rm -f "$account_body"' EXIT

fetch_account() {
  curl -sS --max-time 30 --retry 3 --retry-all-errors -o "$account_body" \
    -w '%{http_code}' "$HORIZON_URL/accounts/$address" 2>/dev/null || echo "000"
}

http_code="$(fetch_account)"
if [[ "$http_code" == "404" ]]; then
  [[ -n "$friendbot_url" ]] \
    || infra_fail "source account $address does not exist on $NETWORK and the network has no friendbot; fund it manually"
  printf '[WARN] source account %s not found on %s — the network was reset or the key was never funded; re-creating it via friendbot\n' "$address" "$NETWORK" >&2
  fb_body="$(mktemp)"
  fb_code="$(curl -sS --max-time 60 -o "$fb_body" -w '%{http_code}' \
    "${friendbot_url%/}/?addr=$address" 2>/dev/null || echo "000")"
  fb_detail="$(jq -r '.detail // empty' "$fb_body" 2>/dev/null || true)"
  rm -f "$fb_body"
  case "$fb_code" in
    200) pass "re-created source account via friendbot: $address" ;;
    429) infra_fail "friendbot rate-limited funding $address (HTTP 429); retry later" ;;
    *)   infra_fail "friendbot could not fund $address (HTTP $fb_code${fb_detail:+: $fb_detail})" ;;
  esac
  http_code="$(fetch_account)"
fi
[[ "$http_code" == "200" ]] \
  || infra_fail "Horizon $HORIZON_URL could not load account $address (HTTP $http_code)"

# ── 3. Enough balance for a full deployment ────────────────────────────────
balance="$(jq -r '[.balances[] | select(.asset_type == "native") | .balance][0] // "0"' "$account_body")"
if awk -v have="$balance" -v need="$MIN_BALANCE_XLM" 'BEGIN { exit !(have + 0 < need + 0) }'; then
  infra_fail "source account $address holds $balance XLM, below the $MIN_BALANCE_XLM XLM a deployment needs; top it up or rotate TESTNET_SECRET_KEY to a fresh key (friendbot only funds new accounts)"
fi
pass "source account $address funded: $balance XLM"
