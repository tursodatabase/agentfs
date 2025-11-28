#!/bin/sh
set -e

echo -n "TEST interactive bash session... "

TEST_AGENT_ID="test-bash-agent"

# Clean up any existing test database
rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"

# Initialize the database using agentfs init
cargo run -- init "$TEST_AGENT_ID" > /dev/null 2>&1

# Run bash session: write a file and read it back (like README example)
output=$(cargo run -- run --mount type=sqlite,src=".agentfs/${TEST_AGENT_ID}.db",dst=/agent /bin/bash -c '
echo "hello from agent" > /agent/hello.txt
cat /agent/hello.txt
' 2>&1)

# Verify we got the expected output
echo "$output" | grep -q "hello from agent" || {
    echo "FAILED"
    echo "$output"
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
    exit 1
}

# Cleanup test database only
rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"

echo "OK"
