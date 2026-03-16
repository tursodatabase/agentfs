# gRPC Bridge Example (Python)

This example provides:

1. `grpc_server.py`: gRPC implementation of AppFS adapter bridge service.
2. `http_gateway.py`: HTTP gateway exposing `/v1/submit-action` and `/v1/submit-control-action`, forwarding to gRPC backend.

Use the gateway with current runtime (`--adapter-http-endpoint`) while keeping adapter logic behind gRPC.

## 1. Install dependencies

```bash
cd examples/appfs/grpc-bridge/python
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

## 2. Generate Python stubs

```bash
./generate_stubs.sh
```

This generates:

1. `appfs_adapter_v1_pb2.py`
2. `appfs_adapter_v1_pb2_grpc.py`

## 3. Start gRPC server

```bash
python3 grpc_server.py
```

Default listen: `127.0.0.1:50051`.

## 4. Start HTTP gateway

```bash
python3 http_gateway.py
```

Default listen: `127.0.0.1:8080`.

## 5. Start AppFS runtime in bridge mode

```bash
cd cli
agentfs serve appfs \
  --root /app \
  --app-id aiim \
  --adapter-http-endpoint http://127.0.0.1:8080
```

For live harness:

```bash
cd cli
APPFS_ADAPTER_HTTP_ENDPOINT=http://127.0.0.1:8080 \
APPFS_CONTRACT_TESTS=1 \
sh ./tests/appfs/run-live-with-adapter.sh
```
