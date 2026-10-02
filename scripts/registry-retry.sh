#!/usr/bin/env bash
# Retry a registry command only on transient server errors (HTTP 5xx, 429,
# gateway/timeout wording). Source this file and run the command through
# `retry_transient`; it makes at most 3 attempts with growing pauses, logs each
# attempt to stderr, and fails with the last output after the bound. Any other
# error fails at once, never downgraded to a warning.
#
#   RETRY_FIRST  first pause in seconds (default 15); doubles each time
#   OK_PATTERN   optional ERE: a failing run whose output matches counts as
#                success ("already set", "nothing to do"); the only downgrade

retry_transient() {
  local label=$1; shift
  local pause=${RETRY_FIRST:-15} attempt out status
  for attempt in 1 2 3; do
    status=0
    out=$("$@" 2>&1) || status=$?
    if [ "$status" -eq 0 ]; then
      [ -n "$out" ] && printf '%s\n' "$out"
      return 0
    fi
    if [ -n "${OK_PATTERN:-}" ] && grep -qE "$OK_PATTERN" <<<"$out"; then
      printf '%s\n' "$out"
      echo "ok: $label (nothing to do)" >&2
      return 0
    fi
    if ! grep -qiE '(^|[^0-9])(429|5[0-9][0-9])([^0-9]|$)|too many requests|internal server error|bad gateway|service unavailable|gateway time-?out' <<<"$out"; then
      printf '%s\n' "$out" >&2
      echo "FAILED: $label is not a transient registry error (exit $status)" >&2
      return "$status"
    fi
    if [ "$attempt" -eq 3 ]; then
      printf '%s\n' "$out" >&2
      echo "FAILED: $label still failing with a registry server error after 3 attempts" >&2
      return "$status"
    fi
    echo "retry: $label hit a registry server error (attempt $attempt/3); retrying in ${pause}s" >&2
    sleep "$pause"
    pause=$((pause * 2))
  done
}
