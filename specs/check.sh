#!/bin/sh
# Runs the checks of the ingest model (specs/ingest.qnt). See docs/testing/formal-model.md.
#
#   specs/check.sh typecheck   parse and typecheck
#   specs/check.sh simulate    random simulation of the contract: no violation, every witness reached
#   specs/check.sh mutants     random simulation of every mutant: the named property is violated
#   specs/check.sh seeds       replay the recorded seed of every mutant
#   specs/check.sh verify      bounded model checking with Apalache
#   specs/check.sh exhaustive  explicit-state model checking with TLC: safety and liveness on the tiny instance
#                              (needs the Apalache distribution that `verify` downloads)
#   specs/check.sh all         everything above
#
# Environment: SAMPLES (10000), STEPS (60), THREADS (2), VERIFY_STEPS (6).
set -eu
cd "$(dirname "$0")"

SPEC=ingest.qnt
SAMPLES=${SAMPLES:-10000}
STEPS=${STEPS:-60}
THREADS=${THREADS:-2}
VERIFY_STEPS=${VERIFY_STEPS:-6}

SAFETY="AckOnlyAfterSync NoAckedLoss NoDuplicateRows CheckpointMonotonic TruncateOnlyBelowCheckpoint
  BatchIdentityStable ReadYourAck"
WITNESSES="reachedCheckpoint reachedTruncate reachedQuarantine reachedProbePresent reachedProbeAbsent
  reachedDeduplicated reachedWindowExpire reachedReship reachedGap"

# module:property that must be violated:recorded seed
MUTANTS="
mutant_ack_before_sync:AckOnlyAfterSync:0x6e0e16290c7a6342
mutant_send_before_seal:BatchIdentityStable:0x5e222faea438aabf
mutant_drop_token:NoDuplicateRows:0xfc3ff7b7f569e714
mutant_truncate_early:TruncateOnlyBelowCheckpoint:0x2aaa4c6f109fee59
mutant_no_probe:NoDuplicateRows:0x9c79520373fc318d
assumption_short_window:NoDuplicateRows:0x9626299bcef9648e
ingest_small:ReadYourAckStrict:0x3c65ec97ca7ea887
"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Only the named property is checked, so a violation is a violation of that property.
expect_violation() {
  module=$1
  property=$2
  shift 2
  out=$(quint run "$SPEC" --main "$module" --invariant "$property" --max-steps "$STEPS" --verbosity 1 "$@" \
    2>&1) || true
  echo "$out"
  echo "$out" | grep -q '^\[violation\]' || fail "$module: expected a violation of $property"
  echo "OK: $module violates $property"
}

typecheck() {
  quint typecheck "$SPEC"
  echo "OK: typecheck"
}

simulate() {
  # shellcheck disable=SC2086
  out=$(quint run "$SPEC" --main ingest_ok --invariants $SAFETY NoStuckState --witnesses $WITNESSES \
    --max-samples "$SAMPLES" --max-steps "$STEPS" --n-threads "$THREADS" --verbosity 1 2>&1) || {
    echo "$out"
    fail "ingest_ok: violation or error"
  }
  echo "$out"
  echo "$out" | grep -q '^\[ok\]' || fail "ingest_ok: no [ok] line"
  if echo "$out" | grep -q 'witnessed in 0 trace'; then fail "ingest_ok: a witness was never reached"; fi
  echo "OK: ingest_ok holds over $SAMPLES samples of $STEPS steps"
}

mutants() {
  for entry in $MUTANTS; do
    module=${entry%%:*}
    rest=${entry#*:}
    expect_violation "$module" "${rest%%:*}" --max-samples "$SAMPLES" --n-threads "$THREADS"
  done
}

seeds() {
  for entry in $MUTANTS; do
    module=${entry%%:*}
    rest=${entry#*:}
    expect_violation "$module" "${rest%%:*}" --seed "${rest#*:}"
  done
}

verify() {
  out=$(quint verify "$SPEC" --main ingest_small --invariant Safety --max-steps "$VERIFY_STEPS" \
    --verbosity 1 2>&1) || {
    echo "$out"
    fail "ingest_small: violation or error"
  }
  echo "$out"
  echo "$out" | grep -q '^\[ok\]' || fail "ingest_small: no [ok] line"
  echo "OK: ingest_small satisfies Safety up to $VERIFY_STEPS steps"

  out=$(quint verify "$SPEC" --main mutant_ack_before_sync --invariant AckOnlyAfterSync \
    --max-steps "$VERIFY_STEPS" --verbosity 1 2>&1) || true
  echo "$out"
  echo "$out" | grep -q '^\[violation\]' || fail "mutant_ack_before_sync: expected a violation of AckOnlyAfterSync"
  echo "OK: mutant_ack_before_sync violates AckOnlyAfterSync (Apalache)"
}

# TLC explores every reachable state of the tiny instance, so these results have no depth bound. Its raw
# output is long; only the state counts, the temporal check, errors and the verdict are kept.
tlc() {
  quint verify "$SPEC" --backend tlc --tlc-config tlc.json --verbosity 3 "$@" 2>&1 |
    grep -E '^\[(ok|violation|failure)\]|^[0-9]+ states generated|^The depth of|temporal properties|^Error: ' ||
    true
}

exhaustive() {
  out=$(tlc --main ingest_tiny --invariant Safety,NoStuckState)
  echo "$out"
  echo "$out" | grep -q '^\[ok\]' || fail "ingest_tiny: expected Safety and NoStuckState to hold"
  echo "OK: ingest_tiny satisfies Safety and NoStuckState in every reachable state"

  out=$(tlc --main ingest_tiny --temporal AckedEventuallyLands,CoveredEventuallyTruncated)
  echo "$out"
  echo "$out" | grep -q '^\[ok\]' || fail "ingest_tiny: expected the liveness properties to hold"
  echo "OK: ingest_tiny satisfies AckedEventuallyLands and CoveredEventuallyTruncated under Fairness"

  out=$(tlc --main reading_retry_without_reprobe --invariant NoDuplicateRows)
  echo "$out"
  echo "$out" | grep -q '^\[violation\]' ||
    fail "reading_retry_without_reprobe: expected a violation of NoDuplicateRows"
  echo "OK: reading_retry_without_reprobe violates NoDuplicateRows (TLC)"
}

case "${1:-all}" in
  typecheck) typecheck ;;
  simulate) simulate ;;
  mutants) mutants ;;
  seeds) seeds ;;
  verify) verify ;;
  exhaustive) exhaustive ;;
  all)
    typecheck
    simulate
    mutants
    seeds
    verify
    exhaustive
    ;;
  *) fail "unknown mode: $1" ;;
esac
