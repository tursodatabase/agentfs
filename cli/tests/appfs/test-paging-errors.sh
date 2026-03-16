#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=./lib.sh
. "$SCRIPT_DIR/lib.sh"

banner "AppFS CT-009 Paging Error Mapping"

events="$APPFS_APP_DIR/_stream/events.evt.jsonl"
resource="$APPFS_PAGEABLE_RESOURCE"
fetch_next="$APPFS_APP_DIR/_paging/fetch_next.act"
close_act="$APPFS_APP_DIR/_paging/close.act"
expired_resource="${APPFS_EXPIRED_PAGEABLE_RESOURCE:-$APPFS_APP_DIR/chats/chat-expired/messages.res.json}"

assert_file "$events"
assert_file "$fetch_next"
assert_file "$close_act"
assert_file "$resource"
assert_file "$expired_resource"
require_cmd jq

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

wait_for_token_event() {
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

assert_failed_code() {
    token="$1"
    code="$2"
    tmp_file="$(mktemp)"
    grep "$token" "$events" > "$tmp_file" || true
    [ -s "$tmp_file" ] || fail "token event not found: $token"
    tail -n 1 "$tmp_file" | jq -e '.type=="action.failed"' >/dev/null 2>&1 || fail "token $token did not produce action.failed"
    actual_code="$(tail -n 1 "$tmp_file" | jq -r '.error.code')"
    [ "$actual_code" = "$code" ] || fail "token $token expected error.code=$code, got $actual_code"
    rm -f "$tmp_file"
    pass "token $token -> action.failed/$code"
}

active_handle="$(jq -r '.page.handle_id' "$resource")"
[ "$active_handle" != "null" ] || fail "active handle missing in $resource"
[ -n "$active_handle" ] || fail "active handle empty in $resource"

expired_handle="$(jq -r '.page.handle_id' "$expired_resource")"
[ "$expired_handle" != "null" ] || fail "expired handle missing in $expired_resource"
[ -n "$expired_handle" ] || fail "expired handle empty in $expired_resource"

token_unknown="ct-page-unknown-$$"
wait_writable "$fetch_next" || fail "action sink remained non-writable: $fetch_next"
printf '{"handle_id":"ph_not_found","client_token":"%s"}\n' "$token_unknown" > "$fetch_next" || fail "unknown handle submit failed"
wait_for_token_event "$token_unknown" || fail "unknown-handle event did not arrive in time"
assert_failed_code "$token_unknown" "PAGER_HANDLE_NOT_FOUND"

token_cross="ct-page-cross-$$"
wait_writable "$fetch_next" || fail "action sink remained non-writable: $fetch_next"
printf '{"handle_id":"%s","session_id":"sess-forbidden","client_token":"%s"}\n' "$active_handle" "$token_cross" > "$fetch_next" || fail "cross-session submit failed"
wait_for_token_event "$token_cross" || fail "cross-session event did not arrive in time"
assert_failed_code "$token_cross" "PERMISSION_DENIED"

token_close="ct-page-close-$$"
wait_writable "$close_act" || fail "action sink remained non-writable: $close_act"
printf '{"handle_id":"%s","client_token":"%s"}\n' "$active_handle" "$token_close" > "$close_act" || fail "close submit failed"
wait_for_token_event "$token_close" || fail "close event did not arrive in time"
pass "close request processed for closed-handle check"

token_closed="ct-page-closed-$$"
wait_writable "$fetch_next" || fail "action sink remained non-writable: $fetch_next"
printf '{"handle_id":"%s","client_token":"%s"}\n' "$active_handle" "$token_closed" > "$fetch_next" || fail "closed handle submit failed"
wait_for_token_event "$token_closed" || fail "closed-handle event did not arrive in time"
assert_failed_code "$token_closed" "PAGER_HANDLE_CLOSED"

token_expired="ct-page-expired-$$"
wait_writable "$fetch_next" || fail "action sink remained non-writable: $fetch_next"
printf '{"handle_id":"%s","client_token":"%s"}\n' "$expired_handle" "$token_expired" > "$fetch_next" || fail "expired handle submit failed"
wait_for_token_event "$token_expired" || fail "expired-handle event did not arrive in time"
assert_failed_code "$token_expired" "PAGER_HANDLE_EXPIRED"

say "CT-009 done"
