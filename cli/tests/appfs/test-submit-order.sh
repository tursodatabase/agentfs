#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=./lib.sh
. "$SCRIPT_DIR/lib.sh"

banner "AppFS CT-008 Submit Ordering and Single Terminal"

events="$APPFS_APP_DIR/_stream/events.evt.jsonl"
action="${APPFS_TEST_ACTION:-$APPFS_APP_DIR/contacts/zhangsan/send_message.act}"
require_cmd jq

assert_file "$events"
assert_file "$action"

wait_writable() {
    path="$1"
    i=0
    while [ "$i" -lt "$APPFS_TIMEOUT_SEC" ]; do
        if [ -w "$path" ]; then
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    return 1
}

wait_token_event() {
    token="$1"
    deadline=$(( $(date +%s) + $APPFS_TIMEOUT_SEC ))
    while :; do
        count="$(grep -c "$token" "$events" 2>/dev/null || true)"
        [ -n "$count" ] || count=0
        if [ "$count" -ge 1 ]; then
            return 0
        fi
        now="$(date +%s)"
        [ "$now" -lt "$deadline" ] || return 1
        sleep 1
    done
}

token1="ct-order-1-$$"
token2="ct-order-2-$$"

before_lines="$(wc -l < "$events" 2>/dev/null || echo 0)"

wait_writable "$action" || fail "action sink remained non-writable: $action"
printf 'token:%s\nhello-1\n' "$token1" > "$action" || fail "first submit failed"
pass "first submit accepted by filesystem"

# Ensure first submit is already ingested before writing a second payload to
# the same action sink; otherwise fast overwrite can hide the first request.
wait_token_event "$token1" || fail "first submit event did not arrive in time"
pass "first submit event observed"

wait_writable "$action" || fail "action sink remained non-writable before second submit: $action"
printf 'token:%s\nhello-2\n' "$token2" > "$action" || fail "second submit failed"
pass "second submit accepted by filesystem"

wait_token_event "$token2" || fail "second submit event did not arrive in time"
pass "second submit event observed"

after_lines="$(wc -l < "$events" 2>/dev/null || echo 0)"
[ "$after_lines" -gt "$before_lines" ] || fail "event stream did not grow for ordered submits"
pass "event stream grew ($before_lines -> $after_lines)"

tmp1="$(mktemp)"
tmp2="$(mktemp)"
grep "$token1" "$events" > "$tmp1"
grep "$token2" "$events" > "$tmp2"

# Each inline request should produce exactly one terminal event.
count1="$(wc -l < "$tmp1" | tr -d ' ')"
count2="$(wc -l < "$tmp2" | tr -d ' ')"
[ "$count1" = "1" ] || fail "token1 expected exactly one event, got $count1"
[ "$count2" = "1" ] || fail "token2 expected exactly one event, got $count2"
pass "each submit produced one terminal event"

type1="$(jq -r '.type' "$tmp1")"
type2="$(jq -r '.type' "$tmp2")"
[ "$type1" = "action.completed" ] || fail "token1 terminal type mismatch: $type1"
[ "$type2" = "action.completed" ] || fail "token2 terminal type mismatch: $type2"
pass "terminal type is action.completed for both submits"

seq1="$(jq -r '.seq' "$tmp1")"
seq2="$(jq -r '.seq' "$tmp2")"
[ "$seq1" -lt "$seq2" ] || fail "event order mismatch: seq1=$seq1 seq2=$seq2"
pass "submit order preserved in stream sequence"

rid1="$(jq -r '.request_id' "$tmp1")"
rid2="$(jq -r '.request_id' "$tmp2")"
[ "$rid1" != "$rid2" ] || fail "distinct submits unexpectedly share request_id"
pass "distinct submits have distinct request_id"

rm -f "$tmp1" "$tmp2"

say "CT-008 done"
