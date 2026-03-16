#!/bin/sh
set -eu

DIR="$(dirname "$0")"

if [ "${APPFS_CONTRACT_TESTS:-0}" != "1" ]; then
    echo "SKIP appfs-contract (set APPFS_CONTRACT_TESTS=1 to enable)"
    exit 0
fi

echo "Running AppFS contract tests..."

status=0
if [ "${APPFS_STATIC_FIXTURE:-0}" = "1" ]; then
    tests="
    $DIR/appfs/test-layout.sh
    $DIR/appfs/test-stream-replay.sh
    $DIR/appfs/test-manifest-policy.sh
    "
else
    tests="
    $DIR/appfs/test-layout.sh
    $DIR/appfs/test-action-basics.sh
    $DIR/appfs/test-stream-replay.sh
    $DIR/appfs/test-paging.sh
    $DIR/appfs/test-paging-errors.sh
    $DIR/appfs/test-manifest-policy.sh
    $DIR/appfs/test-streaming-lifecycle.sh
    $DIR/appfs/test-submit-reject.sh
    $DIR/appfs/test-submit-order.sh
    "
fi

for t in $tests; do
    if ! sh "$t"; then
        status=1
    fi
done

if [ "$status" -ne 0 ]; then
    echo "AppFS contract tests: FAILED"
    exit "$status"
fi

echo "AppFS contract tests: OK"
