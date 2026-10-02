#!/usr/bin/env bash
# Offline test for registry-retry.sh: a fake command, no registry.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
cat > "$work/fake" <<'F'
#!/usr/bin/env bash
# Fails $FAKE_FAILS times with $FAKE_ERR, then succeeds.
n=$(cat "$FAKE_STATE" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$FAKE_STATE"
if [ "$n" -le "${FAKE_FAILS:-0}" ]; then echo "$FAKE_ERR"; exit 1; fi
echo done
F
chmod +x "$work/fake"
export FAKE_STATE="$work/n" RETRY_FIRST=0
# shellcheck source=registry-retry.sh
. "$here/registry-retry.sh"
run() { rm -f "$FAKE_STATE"; ( retry_transient t "$work/fake" ) 2>&1; }

# 1. HTTP 500 twice, then fine: third attempt succeeds.
out=$(FAKE_FAILS=2 FAKE_ERR="Error: server returned status 500" run); grep -q '^done' <<<"$out"
test "$(cat "$FAKE_STATE")" = 3

# 2. 429 and 503 are transient too.
out=$(FAKE_FAILS=1 FAKE_ERR="HTTP 429 Too Many Requests" run); grep -q '^done' <<<"$out"
out=$(FAKE_FAILS=1 FAKE_ERR="503 Service Unavailable" run); grep -q '^done' <<<"$out"

# 3. Persistent 5xx: exactly 3 attempts, then a clear failure.
rc=0; out=$(FAKE_FAILS=99 FAKE_ERR="status 502 Bad Gateway" run) || rc=$?
test "$rc" != 0 && grep -q 'after 3 attempts' <<<"$out"
test "$(cat "$FAKE_STATE")" = 3

# 4. A real failure (401, 422, ...) is not retried and not downgraded.
rc=0; out=$(FAKE_FAILS=99 FAKE_ERR="error: 401 unauthorized" run) || rc=$?
test "$rc" != 0 && grep -q 'not a transient' <<<"$out"
test "$(cat "$FAKE_STATE")" = 1

# 5. Only the OK_PATTERN ("already set") counts as nothing to do.
rm -f "$FAKE_STATE"
out=$(FAKE_FAILS=99 FAKE_ERR="status already set to the provided values" OK_PATTERN='already set' retry_transient t "$work/fake" 2>&1)
grep -q 'nothing to do' <<<"$out"
rm -f "$FAKE_STATE"; rc=0
out=$(FAKE_FAILS=99 FAKE_ERR="forbidden" OK_PATTERN='already set' retry_transient t "$work/fake" 2>&1) || rc=$?
test "$rc" != 0
echo "registry-retry tests passed"
