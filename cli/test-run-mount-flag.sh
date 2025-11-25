#!/bin/sh
set -e

echo -n "TEST mount syntax... "

# Test 1: Valid bind mount with src,dst
if ! cargo run -- run --mount type=bind,src=/tmp,dst=/data -- /bin/true > /dev/null 2>&1; then
    echo "FAILED: Valid bind mount (type=bind,src=/tmp,dst=/data) should succeed"
    exit 1
fi

# Test 2: Valid bind mount with source,target aliases
if ! cargo run -- run --mount type=bind,source=/tmp,target=/data -- /bin/true > /dev/null 2>&1; then
    echo "FAILED: Valid bind mount with aliases should succeed"
    exit 1
fi

# Test 3: Multiple mounts
if ! output=$(cargo run -- run \
    --mount type=bind,src=/tmp,dst=/data1 \
    --mount type=bind,src=/tmp,dst=/data2 \
    -- /bin/true 2>&1); then
    echo "FAILED: Multiple mounts should succeed"
    exit 1
fi

echo "$output" | grep -q "mount points are sandboxed" || {
    echo "FAILED: Multiple mounts should show mount table"
    exit 1
}

# Test 4: Missing type field (should error)
if cargo run -- run --mount src=/tmp,dst=/data -- /bin/true 2>&1 | grep -q "Missing required field 'type'"; then
    :  # Expected to fail with this error
else
    echo "FAILED: Missing type field should produce error"
    exit 1
fi

# Test 5: Missing src field (should error)
if cargo run -- run --mount type=bind,dst=/data -- /bin/true 2>&1 | grep -q "requires 'src' field"; then
    :  # Expected to fail with this error
else
    echo "FAILED: Missing src field should produce error"
    exit 1
fi

# Test 6: Missing dst field (should error)
if cargo run -- run --mount type=bind,src=/tmp -- /bin/true 2>&1 | grep -q "requires 'dst' field"; then
    :  # Expected to fail with this error
else
    echo "FAILED: Missing dst field should produce error"
    exit 1
fi

# Test 7: Invalid mount type (should error)
if cargo run -- run --mount type=foobar,src=/tmp,dst=/data -- /bin/true 2>&1 | grep -q "Unsupported mount type"; then
    :  # Expected to fail with this error
else
    echo "FAILED: Invalid mount type should produce error"
    exit 1
fi

# Test 8: Invalid key=value format (should error)
if cargo run -- run --mount type=bind,invalid,dst=/data -- /bin/true 2>&1 | grep -q "Invalid mount option"; then
    :  # Expected to fail with this error
else
    echo "FAILED: Invalid key=value format should produce error"
    exit 1
fi

# Test 9: Non-existent source path (should error)
if cargo run -- run --mount type=bind,src=/nonexistent-path-12345,dst=/data -- /bin/true 2>&1 | grep -q "Failed to canonicalize"; then
    :  # Expected to fail with this error
else
    echo "FAILED: Non-existent source path should produce error"
    exit 1
fi

# Test 10: Duplicate keys (should error)
if cargo run -- run --mount type=bind,src=/tmp,src=/var,dst=/data -- /bin/true 2>&1 | grep -q "Duplicate key"; then
    :  # Expected to fail with this error
else
    echo "FAILED: Duplicate keys should produce error"
    exit 1
fi

# Test 11: Relative destination path (should error)
if cargo run -- run --mount type=bind,src=/tmp,dst=relative/path -- /bin/true 2>&1 | grep -q "must be absolute"; then
    :  # Expected to fail with this error
else
    echo "FAILED: Relative destination path should produce error"
    exit 1
fi

echo "OK"
