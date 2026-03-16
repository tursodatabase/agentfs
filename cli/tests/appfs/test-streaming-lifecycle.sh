#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=./lib.sh
. "$SCRIPT_DIR/lib.sh"

banner "AppFS CT-006 Streaming Lifecycle"

events="$APPFS_APP_DIR/_stream/events.evt.jsonl"
action="${APPFS_STREAMING_ACTION:-$APPFS_APP_DIR/files/file-001/download.act}"
require_cmd jq

assert_file "$events"
assert_file "$action"

token="ct-stream-$$"
before_lines="$(wc -l < "$events" 2>/dev/null || echo 0)"

printf '{"target":"/tmp/appfs-download.bin","client_token":"%s"}\n' "$token" > "$action" || fail "write+close failed: $action"
pass "streaming action submitted"

deadline=$(( $(date +%s) + $APPFS_TIMEOUT_SEC ))
while :; do
    count="$(grep -c "$token" "$events" 2>/dev/null || true)"
    [ -n "$count" ] || count=0
    if [ "$count" -ge 3 ]; then
        break
    fi
    now="$(date +%s)"
    [ "$now" -lt "$deadline" ] || fail "streaming lifecycle events did not arrive in time"
    sleep 1
done

after_lines="$(wc -l < "$events" 2>/dev/null || echo 0)"
[ "$after_lines" -gt "$before_lines" ] || fail "event stream did not grow for streaming action"
pass "event stream grew ($before_lines -> $after_lines)"

tmp_file="$(mktemp)"
grep "$token" "$events" > "$tmp_file"

request_count="$(jq -r '.request_id' "$tmp_file" | sort -u | wc -l | tr -d ' ')"
[ "$request_count" = "1" ] || fail "expected single request_id for streaming action, got $request_count"
pass "streaming events share one request_id"

types="$(jq -r '.type' "$tmp_file" | sort -u | tr '\n' ' ')"
echo "$types" | grep -q "action.accepted" || fail "missing action.accepted"
echo "$types" | grep -q "action.progress" || fail "missing action.progress"
echo "$types" | grep -q "action.completed" || fail "missing action.completed"
pass "streaming lifecycle includes accepted/progress/completed"

terminal_count="$(jq -r 'select(.type=="action.completed" or .type=="action.failed" or .type=="action.canceled") | .type' "$tmp_file" | wc -l | tr -d ' ')"
[ "$terminal_count" = "1" ] || fail "expected exactly one terminal event, got $terminal_count"
pass "streaming request has single terminal event"

rm -f "$tmp_file"

say "CT-006 done"
