#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
PROTO_DIR="$SCRIPT_DIR/../proto"

python3 -m grpc_tools.protoc \
  -I "$PROTO_DIR" \
  --python_out="$SCRIPT_DIR" \
  --grpc_python_out="$SCRIPT_DIR" \
  "$PROTO_DIR/appfs_adapter_v1.proto"

echo "Generated stubs:"
echo "  $SCRIPT_DIR/appfs_adapter_v1_pb2.py"
echo "  $SCRIPT_DIR/appfs_adapter_v1_pb2_grpc.py"
