#!/usr/bin/env bash
# Post-publish checks wait for npm to serve a fresh version. `npm publish`
# returns before every registry edge knows the version, so a check run seconds
# later can see ETARGET ("No matching version found") or 404. Source this file
# and run each check through `settle`; it retries only registry-propagation
# failures, with growing pauses, logs every attempt (stderr; a check's own
# output stays on stdout), and fails after the bound. Each check must be an
# executable (it runs under `timeout`), not a shell function or builtin.
#
#   SETTLE_BOUND  total seconds to wait (default 600)
#   SETTLE_FIRST  first pause in seconds (default 5); doubles up to SETTLE_MAX
#   SETTLE_MAX    longest pause in seconds (default 30)

settle() {
  local label=$1; shift
  local bound=${SETTLE_BOUND:-600} pause=${SETTLE_FIRST:-5} cap=${SETTLE_MAX:-30}
  local start=$SECONDS attempt=0 out err status errf
  errf=$(mktemp) || return 1
  while :; do
    attempt=$((attempt + 1))
    status=0
    out=$(timeout 300 "$@" 2>"$errf") || status=$?
    err=$(cat "$errf")
    if [ "$status" -eq 0 ]; then
      rm -f "$errf"
      [ -n "$out" ] && printf '%s\n' "$out"
      [ -n "$err" ] && printf '%s\n' "$err" >&2
      echo "ok: $label (attempt $attempt, $((SECONDS - start))s)" >&2
      return 0
    fi
    if ! grep -qiE 'ETARGET|E404|No matching version|notarget|404 Not Found' <<<"$out$err"; then
      rm -f "$errf"
      printf '%s\n' "$out" "$err" >&2
      echo "FAILED: $label is not a registry delay (exit $status)" >&2
      return "$status"
    fi
    if [ $((SECONDS - start + pause)) -gt "$bound" ]; then
      rm -f "$errf"
      printf '%s\n' "$out" "$err" >&2
      echo "FAILED: $label still not served by the npm registry after $((SECONDS - start))s ($attempt attempts)" >&2
      return 1
    fi
    echo "wait: $label not served yet (attempt $attempt, $((SECONDS - start))s); retrying in ${pause}s" >&2
    sleep "$pause"
    pause=$((pause * 2))
    [ "$pause" -gt "$cap" ] && pause=$cap
  done
}

# Every package directory given (natives, main, aliases) must resolve at $1.
# Readiness signal: `npm view name@V version`.
settle_packages() {
  local version=$1 dir name; shift
  for dir in "$@"; do
    name=$(node -p "require('./${dir%/}/package.json').name") || return 1
    settle "$name@$version resolves" npm view "$name@$version" version || return 1
  done
}
