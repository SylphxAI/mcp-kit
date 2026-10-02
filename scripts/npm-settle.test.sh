#!/usr/bin/env bash
# Offline test for npm-settle.sh: a fake `npm`/`npx` on PATH, no registry.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
cat > "$work/bin/fake" <<'F'
#!/usr/bin/env bash
# Fails $FAKE_FAILS times with $FAKE_ERR, then succeeds.
n=$(cat "$FAKE_STATE" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$FAKE_STATE"
if [ "$n" -le "${FAKE_FAILS:-0}" ]; then echo "$FAKE_ERR" >&2; exit 1; fi
echo "1.2.3"
F
chmod +x "$work/bin/fake"
export FAKE_STATE="$work/n"
# shellcheck source=npm-settle.sh
. "$here/npm-settle.sh"
export SETTLE_FIRST=1 SETTLE_MAX=2 SETTLE_BOUND=60
run() { rm -f "$FAKE_STATE"; ( settle "t" "$work/bin/fake" ) 2>&1; }

# 1. ETARGET twice, then served: succeeds on attempt 3.
out=$(FAKE_FAILS=2 FAKE_ERR="npm error code ETARGET
npm error notarget No matching version found for @x/y@1.2.3." run)
grep -q 'attempt 3' <<<"$out" && grep -q '^ok:' <<<"$out"
test "$(grep -c '^wait:' <<<"$out")" = 2

# 2. A 404 is retried too.
out=$(FAKE_FAILS=1 FAKE_ERR="npm error code E404" run); grep -q '^ok:' <<<"$out"

# 3. Other errors are not retried.
rc=0; out=$(FAKE_FAILS=5 FAKE_ERR="npm error code EACCES" run) || rc=$?
test "$rc" != 0 && grep -q 'not a registry delay' <<<"$out"
test "$(cat "$FAKE_STATE")" = 1

# 4. Never served: fails clearly after the bound.
rc=0; out=$(SETTLE_BOUND=3 FAKE_FAILS=999 FAKE_ERR="ETARGET" run) || rc=$?
test "$rc" != 0 && grep -q 'still not served' <<<"$out"

# 5. settle_packages checks every package dir.
mkdir -p "$work/p/a" "$work/p/b"
echo '{"name":"@x/a"}' > "$work/p/a/package.json"; echo '{"name":"@x/b"}' > "$work/p/b/package.json"
cat > "$work/bin/npm" <<'N'
#!/usr/bin/env bash
echo "$2" >> "$FAKE_LOG"; echo 1.2.3
N
chmod +x "$work/bin/npm"; export FAKE_LOG="$work/log"
( cd "$work" && PATH="$work/bin:$PATH" settle_packages 1.2.3 p/a p/b >/dev/null )
test "$(tr '\n' ' ' < "$FAKE_LOG")" = "@x/a@1.2.3 @x/b@1.2.3 "

# 6. A stderr warning on success never leaks into captured stdout.
cat > "$work/bin/warn" <<'W'
#!/usr/bin/env bash
echo "npm warn something" >&2; echo "1.2.3"
W
chmod +x "$work/bin/warn"
actual=$(settle t "$work/bin/warn" 2>/dev/null)
test "$actual" = "1.2.3"
actual=$(PATH="$work/bin:$PATH" settle t warn 2>/dev/null)
test "$actual" = "1.2.3"
echo "npm-settle tests passed"
