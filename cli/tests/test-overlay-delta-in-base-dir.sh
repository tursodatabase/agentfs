#!/bin/sh
set -e

echo -n "TEST overlay readdir/unlink for delta files in base directories... "

TEST_AGENT_ID="test-overlay-delta-in-base-dir-agent"
MOUNTPOINT="/tmp/agentfs-test-overlay-mount-$$"
BASEDIR="/tmp/agentfs-test-overlay-base-$$"

cleanup() {
    # Unmount if mounted
    fusermount -u "$MOUNTPOINT" 2>/dev/null || true
    # Remove directories
    rm -rf "$MOUNTPOINT" "$BASEDIR" 2>/dev/null || true
    # Remove test database
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
}

# Ensure cleanup on exit
trap cleanup EXIT

# Clean up any existing test artifacts
cleanup

# Create base directory with a subdirectory (simulating .git)
mkdir -p "$BASEDIR/.git"
echo "[core]" > "$BASEDIR/.git/config"
echo "ref: refs/heads/main" > "$BASEDIR/.git/HEAD"

# Initialize the database with --base for overlay
if ! output=$(cargo run -- init "$TEST_AGENT_ID" --base "$BASEDIR" 2>&1); then
    echo "FAILED: init with --base failed"
    echo "Output was: $output"
    exit 1
fi

# Create mountpoint
mkdir -p "$MOUNTPOINT"

# Mount in foreground mode (background it ourselves so we can control it)
cargo run -- mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground &
MOUNT_PID=$!

# Wait for mount to be ready
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

# Verify base directory structure is visible
if [ ! -d "$MOUNTPOINT/.git" ]; then
    echo "FAILED: base .git directory not visible through overlay"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

if [ ! -f "$MOUNTPOINT/.git/config" ]; then
    echo "FAILED: base .git/config file not visible through overlay"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Create a new file in the base subdirectory through the overlay
# This triggers ensure_parent_dirs which creates .git in delta with origin mapping
echo "lock content" > "$MOUNTPOINT/.git/index.lock"

# Verify the file was created
if [ ! -f "$MOUNTPOINT/.git/index.lock" ]; then
    echo "FAILED: could not create index.lock in .git directory"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Verify readdir shows both base and delta files
# This is the first bug: delta files in base directories were invisible in readdir
LS_OUTPUT=$(ls "$MOUNTPOINT/.git")
if ! echo "$LS_OUTPUT" | grep -q "index.lock"; then
    echo "FAILED: readdir does not show delta file index.lock"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

if ! echo "$LS_OUTPUT" | grep -q "config"; then
    echo "FAILED: readdir does not show base file config"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

if ! echo "$LS_OUTPUT" | grep -q "HEAD"; then
    echo "FAILED: readdir does not show base file HEAD"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Delete the delta file
# This is the second bug: unlink failed for delta files in base directories
rm "$MOUNTPOINT/.git/index.lock"

# Verify the file is actually deleted
if [ -f "$MOUNTPOINT/.git/index.lock" ]; then
    echo "FAILED: index.lock still exists after deletion"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Verify readdir no longer shows it
LS_OUTPUT_AFTER=$(ls "$MOUNTPOINT/.git")
if echo "$LS_OUTPUT_AFTER" | grep -q "index.lock"; then
    echo "FAILED: readdir still shows index.lock after deletion"
    echo "ls output was: $LS_OUTPUT_AFTER"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Base files should still be visible
if ! echo "$LS_OUTPUT_AFTER" | grep -q "config"; then
    echo "FAILED: base file config disappeared after delta file deletion"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test creating and deleting a subdirectory in a base directory
mkdir "$MOUNTPOINT/.git/objects"
if [ ! -d "$MOUNTPOINT/.git/objects" ]; then
    echo "FAILED: could not create objects subdirectory in .git"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Verify readdir shows the new directory
LS_WITH_DIR=$(ls "$MOUNTPOINT/.git")
if ! echo "$LS_WITH_DIR" | grep -q "objects"; then
    echo "FAILED: readdir does not show delta directory objects"
    echo "ls output was: $LS_WITH_DIR"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Remove the directory (rmdir)
rmdir "$MOUNTPOINT/.git/objects"
if [ -d "$MOUNTPOINT/.git/objects" ]; then
    echo "FAILED: objects directory still exists after rmdir"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Unmount
fusermount -u "$MOUNTPOINT"
wait $MOUNT_PID 2>/dev/null || true

echo "OK"
