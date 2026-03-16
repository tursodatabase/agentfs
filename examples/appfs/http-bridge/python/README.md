# Python HTTP Bridge Example

This example shows how to implement an out-of-process AppFS adapter in Python.

## Run bridge service

```bash
cd examples/appfs/http-bridge/python
python3 bridge_server.py
```

## Run AppFS runtime with bridge mode

```bash
cd cli
cargo run -- serve appfs \
  --root /app \
  --app-id aiim \
  --adapter-http-endpoint http://127.0.0.1:8080 \
  --adapter-http-timeout-ms 5000
```

## Bridge contract

Runtime sends requests to:

1. `POST /v1/submit-action`
2. `POST /v1/submit-control-action`

Response payloads should match AppFS adapter SDK result shapes:

1. `AdapterSubmitOutcomeV1`
2. `AdapterControlOutcomeV1`
3. `AdapterErrorV1` (or `{code,message,retryable}` fallback)
