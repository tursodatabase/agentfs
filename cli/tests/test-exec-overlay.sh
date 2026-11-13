#!/bin/sh
set -e

echo -n "TEST exec with overlay base... "

TEST_AGENT_ID="test-exec-overlay-agent"
BASEDIR="/tmp/agentfs-test-exec-base-$$"

cleanup() {
    rm -rf "$BASEDIR" 2>/dev/null || true
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
}

trap cleanup EXIT

cleanup

mkdir -p "$BASEDIR"
echo "original content" > "$BASEDIR/testfile.txt"

if ! output=$(cargo run -- init "$TEST_AGENT_ID" --base "$BASEDIR" 2>&1); then
    echo "FAILED: init with --base failed"
    echo "Output was: $output"
    exit 1
fi

if ! output=$(timeout 60 cargo run -- exec "$TEST_AGENT_ID" cat testfile.txt 2>&1); then
    echo "FAILED: exec failed"
    echo "Output was: $output"
    exit 1
fi

if ! echo "$output" | grep -q "^original content$"; then
    echo "FAILED: base file not readable through exec mount"
    echo "Output was: $output"
    exit 1
fi

echo "OK"
