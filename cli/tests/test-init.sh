#!/bin/sh
set -e

echo -n "TEST init... "

TEST_AGENT_ID="test-agent"

# Cleanup any existing test database (not the entire .agentfs directory!)
rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"

# Test: Run init command with specific ID
if ! output=$(cargo run -- init "$TEST_AGENT_ID" 2>&1); then
    echo "FAILED: init command failed"
    echo "Output was: $output"
    exit 1
fi

# Check that .agentfs directory was created
if [ ! -d .agentfs ]; then
    echo "FAILED: .agentfs directory was not created"
    echo "Output was: $output"
    exit 1
fi

# Check that the database file was created in .agentfs
if [ ! -f ".agentfs/$TEST_AGENT_ID.db" ]; then
    echo "FAILED: agent database was not created in .agentfs directory"
    echo "Output was: $output"
    exit 1
fi

# Check that output contains success message with .agentfs path
echo "$output" | grep -q "Created agent filesystem: .agentfs/$TEST_AGENT_ID.db" || {
    echo "FAILED: Expected success message not found in output"
    echo "Output was: $output"
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
    exit 1
}

# Test: Running init again should fail without --force
if cargo run -- init "$TEST_AGENT_ID" 2>&1 | grep -q "already exists"; then
    : # Expected behavior
else
    echo "FAILED: init should fail when agent database already exists"
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
    exit 1
fi

# Test: Running init with --force should succeed
if ! output=$(cargo run -- init "$TEST_AGENT_ID" --force 2>&1); then
    echo "FAILED: init --force command failed"
    echo "Output was: $output"
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
    exit 1
fi

# Check that output contains success message
echo "$output" | grep -q "Created agent filesystem: .agentfs/$TEST_AGENT_ID.db" || {
    echo "FAILED: Expected success message not found in init --force output"
    echo "Output was: $output"
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
    exit 1
}

# Cleanup test database only
rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"

echo "OK"
