#!/bin/sh
set -e

DIR="$(dirname "$0")"

"$DIR/test-init.sh"

# Syscall tests in three configurations:
# 1. Linux baseline - establishes expected behavior
"$DIR/test-linux-syscalls.sh"

# 2. ptrace-based sandbox (--experimental-sandbox)
# TODO: The test cases don't currently pass with ptrace-based virtualization
# because of compatibility issues.
# "$DIR/test-run-experimental-syscalls.sh"

# 3. FUSE overlay (agentfs run) - tests copy-on-write
"$DIR/test-run-syscalls.sh" || true  # Requires user namespaces (may fail in CI)

"$DIR/test-run-bash.sh" || true  # Requires user namespaces (may fail in CI)
"$DIR/test-run-git.sh" || true  # Requires user namespaces (may fail in CI)
"$DIR/test-mount.sh"
"$DIR/test-overlay-whiteout.sh"
"$DIR/test-overlay-delta-in-base-dir.sh"
"$DIR/test-symlinks.sh" || true  # Requires user namespaces (may fail in CI)
