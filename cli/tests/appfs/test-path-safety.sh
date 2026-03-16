#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=./lib.sh
. "$SCRIPT_DIR/lib.sh"

banner "AppFS CT-012 Path Safety Guard (No Side Effects)"

events="$APPFS_APP_DIR/_stream/events.evt.jsonl"
assert_file "$events"

submit_unsafe_expect_no_event() {
    action_path="$1"
    token="$2"

    mkdir -p "$(dirname "$action_path")"
    before_lines="$(wc -l < "$events" 2>/dev/null || echo 0)"
    printf 'token:%s\nunsafe-probe\n' "$token" > "$action_path" || fail "unsafe submit write failed: $action_path"
    sleep 2
    after_lines="$(wc -l < "$events" 2>/dev/null || echo 0)"
    [ "$after_lines" -eq "$before_lines" ] || fail "unsafe path unexpectedly produced stream event: $action_path"

    token_hits="$(grep -c "$token" "$events" 2>/dev/null || true)"
    [ -n "$token_hits" ] || token_hits=0
    [ "$token_hits" -eq 0 ] || fail "unsafe path token unexpectedly observed in stream: $token"

    pass "unsafe path rejected without side effects: $action_path"
}

unsafe_drive="$APPFS_APP_DIR/contacts/C:/send_message.act"
unsafe_reserved="$APPFS_APP_DIR/contacts/CON/send_message.act"
unsafe_backslash="$APPFS_APP_DIR/contacts/bad\\segment/send_message.act"

submit_unsafe_expect_no_event "$unsafe_drive" "ct-safe-drive-$$"
submit_unsafe_expect_no_event "$unsafe_reserved" "ct-safe-reserved-$$"
submit_unsafe_expect_no_event "$unsafe_backslash" "ct-safe-backslash-$$"

say "CT-012 done"
