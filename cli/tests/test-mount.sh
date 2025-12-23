#!/bin/sh
set -e

echo -n "TEST mount... "

TEST_AGENT_ID="test-mount-agent"
MOUNTPOINT="/tmp/agentfs-test-mount-$$"

cleanup() {
    # Unmount if mounted
    fusermount -u "$MOUNTPOINT" 2>/dev/null || true
    # Remove mountpoint
    rmdir "$MOUNTPOINT" 2>/dev/null || true
    # Remove test database
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
}

# Ensure cleanup on exit
trap cleanup EXIT

# Clean up any existing test artifacts
cleanup

# Initialize the database
cargo run -- init "$TEST_AGENT_ID" > /dev/null 2>&1

# Create mountpoint
mkdir -p "$MOUNTPOINT"

# Mount in foreground mode (background it ourselves so we can control it)
cargo run -- mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground &
MOUNT_PID=$!

# Wait for mount to be ready (check if mountpoint has different device ID)
MAX_WAIT=10
WAITED=0
while [ $WAITED -lt $MAX_WAIT ]; do
    if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
        break
    fi
    sleep 0.5
    WAITED=$((WAITED + 1))
done

if ! mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "FAILED: mount did not become ready in time"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Write a file through the FUSE mount
echo "hello from fuse mount" > "$MOUNTPOINT/hello.txt"

# Read it back
CONTENT=$(cat "$MOUNTPOINT/hello.txt")

if [ "$CONTENT" != "hello from fuse mount" ]; then
    echo "FAILED: file content mismatch"
    echo "Expected: hello from fuse mount"
    echo "Got: $CONTENT"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test mkdir
mkdir "$MOUNTPOINT/testdir"
if [ ! -d "$MOUNTPOINT/testdir" ]; then
    echo "FAILED: mkdir did not create directory"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test creating file in subdirectory
echo "nested file" > "$MOUNTPOINT/testdir/nested.txt"
NESTED_CONTENT=$(cat "$MOUNTPOINT/testdir/nested.txt")

if [ "$NESTED_CONTENT" != "nested file" ]; then
    echo "FAILED: nested file content mismatch"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test symlink creation
ln -s nested.txt "$MOUNTPOINT/testdir/link_to_nested"
if [ ! -L "$MOUNTPOINT/testdir/link_to_nested" ]; then
    echo "FAILED: symlink was not created"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test reading symlink target
LINK_TARGET=$(readlink "$MOUNTPOINT/testdir/link_to_nested")
if [ "$LINK_TARGET" != "nested.txt" ]; then
    echo "FAILED: symlink target mismatch"
    echo "Expected: nested.txt"
    echo "Got: $LINK_TARGET"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test following symlink to read file
LINKED_CONTENT=$(cat "$MOUNTPOINT/testdir/link_to_nested")
if [ "$LINKED_CONTENT" != "nested file" ]; then
    echo "FAILED: reading through symlink failed"
    echo "Expected: nested file"
    echo "Got: $LINKED_CONTENT"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test symlink to directory
ln -s testdir "$MOUNTPOINT/link_to_testdir"
if [ ! -L "$MOUNTPOINT/link_to_testdir" ]; then
    echo "FAILED: symlink to directory was not created"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test accessing file through directory symlink
DIR_LINKED_CONTENT=$(cat "$MOUNTPOINT/link_to_testdir/nested.txt")
if [ "$DIR_LINKED_CONTENT" != "nested file" ]; then
    echo "FAILED: reading through directory symlink failed"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Unmount
fusermount -u "$MOUNTPOINT"

# Wait for mount process to exit
wait $MOUNT_PID 2>/dev/null || true

echo "OK"
