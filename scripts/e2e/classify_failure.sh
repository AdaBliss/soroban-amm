#!/usr/bin/env bash
# classify_failure.sh — decide whether a failed e2e run failed because of the
# network (infrastructure) or because of the code under test (regression).
#
# Usage:
#   bash scripts/e2e/classify_failure.sh e2e.log
#
# Prints two lines, suitable for appending to $GITHUB_OUTPUT:
#   category=infrastructure|regression
#   reason=<the signature that matched, with the log line that matched it>
#
# Only transport- and transaction-envelope-level failures count as
# infrastructure: HTTP rate limits and gateway errors from the RPC, connection
# failures, and the tx-level result codes a deployment account produces when
# it is drained or used by two runs at once. Contract errors, failed
# assertions and anything unrecognised are reported as a regression — a false
# "regression" gets looked at, a false "infrastructure" gets ignored.
set -Eeuo pipefail

log_file="${1:?usage: classify_failure.sh <log file>}"

# label|extended regex (matched case-insensitively)
SIGNATURES=(
  "RPC rate limit|(HTTP|status)[^0-9]{0,10}429|too many requests"
  "RPC unavailable|(HTTP|status)[^0-9]{0,10}(502|503|504)|service unavailable|bad gateway|gateway time-?out"
  "network connection failure|connection (refused|reset|closed)|could not resolve host|dns error|error sending request|operation timed out"
  "transaction not accepted by the network|TRY_AGAIN_LATER|txTooLate|tx_too_late"
  "source account underfunded|txInsufficientBalance|tx_insufficient_balance|txInsufficientFee|tx_insufficient_fee"
  "source account used concurrently|txBadSeq|tx_bad_seq"
  "source account missing (network reset?)|txNoAccount|tx_no_source_account|Account not found"
)

if [[ ! -s "$log_file" ]]; then
  printf 'category=regression\nreason=no e2e output was captured\n'
  exit 0
fi

for entry in "${SIGNATURES[@]}"; do
  label="${entry%%|*}"
  pattern="${entry#*|}"
  if line="$(grep -Eim1 -- "$pattern" "$log_file")"; then
    line="$(tr -d '\r' <<<"$line" | cut -c1-200)"
    printf 'category=infrastructure\nreason=%s — matched: %s\n' "$label" "$line"
    exit 0
  fi
done

failed="$(grep -E '^\[FAIL\]' "$log_file" | head -n 1 | tr -d '\r' | cut -c1-200 || true)"
printf 'category=regression\nreason=%s\n' "${failed:-e2e suite failed without a recognised infrastructure error}"
