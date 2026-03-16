#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
CLI_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)"
REPO_DIR="$(CDPATH= cd -- "$CLI_DIR/.." && pwd)"

APPFS_FIXTURE_DIR="${APPFS_FIXTURE_DIR:-$REPO_DIR/examples/appfs}"
APPFS_LIVE_AGENT_ID="${APPFS_LIVE_AGENT_ID:-appfs-live-$$}"
APPFS_LIVE_MOUNTPOINT="${APPFS_LIVE_MOUNTPOINT:-/tmp/agentfs-appfs-live-$$}"
APPFS_APP_ID="${APPFS_APP_ID:-aiim}"
APPFS_ADAPTER_POLL_MS="${APPFS_ADAPTER_POLL_MS:-100}"
APPFS_ADAPTER_RECONCILE_POLL_MS="${APPFS_ADAPTER_RECONCILE_POLL_MS:-1000}"
APPFS_ADAPTER_HTTP_ENDPOINT="${APPFS_ADAPTER_HTTP_ENDPOINT:-}"
APPFS_ADAPTER_HTTP_TIMEOUT_MS="${APPFS_ADAPTER_HTTP_TIMEOUT_MS:-5000}"
APPFS_TIMEOUT_SEC="${APPFS_TIMEOUT_SEC:-20}"
APPFS_MOUNT_WAIT_SEC="${APPFS_MOUNT_WAIT_SEC:-20}"
APPFS_MOUNT_LOG="${APPFS_MOUNT_LOG:-$CLI_DIR/appfs-mount-live.log}"
APPFS_ADAPTER_LOG="${APPFS_ADAPTER_LOG:-$CLI_DIR/appfs-adapter-live.log}"

MOUNT_PID=""
ADAPTER_PID=""

say() {
    printf '%s\n' "$*"
}

fail() {
    say "FAILED: $*"
    exit 1
}

start_adapter() {
    poll_ms="${1:-$APPFS_ADAPTER_POLL_MS}"
    say "Starting AppFS adapter runtime..."
    set -- "$AGENTFS_BIN" serve appfs --root "$APPFS_LIVE_MOUNTPOINT" --app-id "$APPFS_APP_ID" --poll-ms "$poll_ms"
    if [ -n "$APPFS_ADAPTER_HTTP_ENDPOINT" ]; then
        set -- "$@" --adapter-http-endpoint "$APPFS_ADAPTER_HTTP_ENDPOINT" --adapter-http-timeout-ms "$APPFS_ADAPTER_HTTP_TIMEOUT_MS"
    fi
    "$@" >"$APPFS_ADAPTER_LOG" 2>&1 &
    ADAPTER_PID=$!
    sleep 1
    if ! kill -0 "$ADAPTER_PID" 2>/dev/null; then
        tail -n 80 "$APPFS_ADAPTER_LOG" 2>/dev/null || true
        fail "adapter failed to start"
    fi
}

stop_adapter() {
    if [ -n "${ADAPTER_PID:-}" ] && kill -0 "$ADAPTER_PID" 2>/dev/null; then
        kill "$ADAPTER_PID" 2>/dev/null || true
        wait "$ADAPTER_PID" 2>/dev/null || true
    fi
    ADAPTER_PID=""
}

wait_token_in_events() {
    token="$1"
    file="$2"
    timeout="${3:-20}"
    i=0
    while [ "$i" -lt "$timeout" ]; do
        count="$(grep -c "$token" "$file" 2>/dev/null || true)"
        [ -n "$count" ] || count=0
        if [ "$count" -ge 1 ]; then
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    return 1
}

wait_token_type_count() {
    token="$1"
    event_type="$2"
    min_count="$3"
    file="$4"
    timeout="${5:-20}"
    i=0
    while [ "$i" -lt "$timeout" ]; do
        count="$(grep "$token" "$file" 2>/dev/null | grep -c "\"type\":\"$event_type\"" || true)"
        [ -n "$count" ] || count=0
        if [ "$count" -ge "$min_count" ]; then
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    return 1
}

token_terminal_count() {
    token="$1"
    file="$2"
    grep "$token" "$file" 2>/dev/null | grep -E -c '"type":"action\.(completed|failed|canceled)"' || true
}

wait_writable() {
    path="$1"
    timeout="${2:-20}"
    i=0
    while [ "$i" -lt "$timeout" ]; do
        if [ -w "$path" ]; then
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    return 1
}

cleanup() {
    set +e

    stop_adapter

    if mountpoint -q "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null; then
        fusermount -u "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null || true
    fi

    if [ -n "${MOUNT_PID:-}" ] && kill -0 "$MOUNT_PID" 2>/dev/null; then
        kill "$MOUNT_PID" 2>/dev/null || true
        wait "$MOUNT_PID" 2>/dev/null || true
    fi

    if mountpoint -q "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null; then
        umount "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null || true
    fi

    rmdir "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null || true

    rm -f "$CLI_DIR/.agentfs/${APPFS_LIVE_AGENT_ID}.db"
    rm -f "$CLI_DIR/.agentfs/${APPFS_LIVE_AGENT_ID}.db-shm"
    rm -f "$CLI_DIR/.agentfs/${APPFS_LIVE_AGENT_ID}.db-wal"
}

trap cleanup EXIT INT TERM

command -v cargo >/dev/null 2>&1 || fail "missing command: cargo"
command -v mountpoint >/dev/null 2>&1 || fail "missing command: mountpoint"
command -v fusermount >/dev/null 2>&1 || fail "missing command: fusermount"

[ -d "$APPFS_FIXTURE_DIR" ] || fail "missing fixture directory: $APPFS_FIXTURE_DIR"

cd "$CLI_DIR"

say "Building agentfs binary..."
cargo build >/dev/null
AGENTFS_BIN="$CLI_DIR/target/debug/agentfs"
[ -x "$AGENTFS_BIN" ] || fail "agentfs binary not found: $AGENTFS_BIN"

say "Preparing live mountpoint: $APPFS_LIVE_MOUNTPOINT"
mkdir -p "$APPFS_LIVE_MOUNTPOINT"
if mountpoint -q "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null; then
    fusermount -u "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null || true
fi
find "$APPFS_LIVE_MOUNTPOINT" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null || true

say "Initializing test agent: $APPFS_LIVE_AGENT_ID"
"$AGENTFS_BIN" init "$APPFS_LIVE_AGENT_ID" --force >/dev/null

say "Starting foreground mount process..."
"$AGENTFS_BIN" mount "$APPFS_LIVE_AGENT_ID" "$APPFS_LIVE_MOUNTPOINT" --backend fuse --foreground >"$APPFS_MOUNT_LOG" 2>&1 &
MOUNT_PID=$!

waited=0
while [ "$waited" -lt "$APPFS_MOUNT_WAIT_SEC" ]; do
    if mountpoint -q "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null; then
        break
    fi
    sleep 1
    waited=$((waited + 1))
done
if ! mountpoint -q "$APPFS_LIVE_MOUNTPOINT" 2>/dev/null; then
    tail -n 80 "$APPFS_MOUNT_LOG" 2>/dev/null || true
    fail "mount did not become ready within ${APPFS_MOUNT_WAIT_SEC}s"
fi

say "Copying AppFS fixture into mounted filesystem..."
cp -a "$APPFS_FIXTURE_DIR"/. "$APPFS_LIVE_MOUNTPOINT"/

start_adapter

say "Running AppFS contract tests against live adapter..."
if ! APPFS_CONTRACT_TESTS=1 APPFS_ROOT="$APPFS_LIVE_MOUNTPOINT" APPFS_APP_ID="$APPFS_APP_ID" APPFS_TIMEOUT_SEC="$APPFS_TIMEOUT_SEC" sh "$CLI_DIR/tests/test-appfs-contract.sh"; then
    say "---- mount log tail ----"
    tail -n 80 "$APPFS_MOUNT_LOG" 2>/dev/null || true
    say "---- adapter log tail ----"
    tail -n 80 "$APPFS_ADAPTER_LOG" 2>/dev/null || true
    fail "live AppFS contract tests failed"
fi

say "Lifecycle probe: graceful stop + restart + post-restart submit..."
if ! kill -0 "$ADAPTER_PID" 2>/dev/null; then
    fail "adapter not alive before lifecycle probe"
fi
stop_adapter
if [ -n "${ADAPTER_PID:-}" ] && kill -0 "$ADAPTER_PID" 2>/dev/null; then
    fail "adapter still alive after stop signal"
fi

start_adapter
events_file="$APPFS_LIVE_MOUNTPOINT/$APPFS_APP_ID/_stream/events.evt.jsonl"
probe_action="$APPFS_LIVE_MOUNTPOINT/$APPFS_APP_ID/contacts/lifecycle/send_message.act"
probe_token="ct-lifecycle-$$"
mkdir -p "$(dirname "$probe_action")"
printf 'token:%s\nrestart-ok\n' "$probe_token" > "$probe_action" || fail "lifecycle probe submit failed"
wait_token_in_events "$probe_token" "$events_file" "$APPFS_TIMEOUT_SEC" || fail "lifecycle probe event not observed after adapter restart"
say "Lifecycle probe passed."

say "CT-016: restart reconciliation for accepted-but-not-terminal streaming request..."
stop_adapter
start_adapter "$APPFS_ADAPTER_RECONCILE_POLL_MS"
reconcile_action="${APPFS_STREAMING_ACTION:-$APPFS_LIVE_MOUNTPOINT/$APPFS_APP_ID/files/file-001/download.act}"
reconcile_token="ct-reconcile-$$"
wait_writable "$reconcile_action" "$APPFS_TIMEOUT_SEC" || fail "reconcile action sink not writable: $reconcile_action"
printf '{"target":"/tmp/reconcile.bin","client_token":"%s"}\n' "$reconcile_token" > "$reconcile_action" || fail "reconcile submit failed"
wait_token_type_count "$reconcile_token" "action.accepted" 1 "$events_file" "$APPFS_TIMEOUT_SEC" || fail "reconcile accepted event missing before restart"
terminal_before="$(token_terminal_count "$reconcile_token" "$events_file")"
[ "$terminal_before" -eq 0 ] || fail "reconcile terminal emitted too early before restart"

stop_adapter
start_adapter "$APPFS_ADAPTER_RECONCILE_POLL_MS"
wait_token_type_count "$reconcile_token" "action.progress" 1 "$events_file" "$APPFS_TIMEOUT_SEC" || fail "reconcile progress missing after restart"
wait_token_type_count "$reconcile_token" "action.completed" 1 "$events_file" "$APPFS_TIMEOUT_SEC" || fail "reconcile terminal missing after restart"
terminal_after="$(token_terminal_count "$reconcile_token" "$events_file")"
[ "$terminal_after" -eq 1 ] || fail "reconcile request emitted unexpected terminal count: $terminal_after"
say "CT-016 passed."

say "LIVE AppFS contract tests passed."
