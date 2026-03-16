# AppFS Adapter gRPC Bridge v0.1 (Reference)

- Version: `0.1-draft`
- Date: `2026-03-16`
- Status: `Draft`
- Scope: Optional transport example mapped to frozen `AppAdapterV1` semantics

## 1. Purpose

This document defines a gRPC bridge contract for adapter implementations in any language.

It does not change AppFS Core protocol semantics. Runtime-side guarantees still remain in `agentfs serve appfs`.

## 2. Proto Contract

Reference proto:

- `examples/appfs/grpc-bridge/proto/appfs_adapter_v1.proto`

Service:

1. `SubmitAction`
2. `SubmitControlAction`

## 3. JSON Payload Encoding Rule

To keep bridge messages language-neutral:

1. `Completed`/`Streaming`/`ControlCompleted` content fields are encoded as JSON strings in proto.
2. Bridge implementations must encode valid JSON text for these fields.
3. Runtime-side bridge adapters (or HTTP gateways) parse/forward the JSON as `AdapterSubmitOutcomeV1` / `AdapterControlOutcomeV1`.

## 4. Error Mapping

gRPC service returns bridge-level `Error` in response `oneof`:

1. `code`
2. `message`
3. `retryable`

Mapping target:

1. `AdapterErrorV1::Rejected` for domain validation/permission/type errors.
2. Transport/internal failures map to `AdapterErrorV1::Internal`.

## 5. Deployment Shapes

1. Direct runtime gRPC adapter (supported via `--adapter-grpc-endpoint`).
2. gRPC backend + HTTP gateway sidecar (supported via `--adapter-http-endpoint`).

## 6. Runtime Option

```bash
agentfs serve appfs \
  --root /app \
  --app-id aiim \
  --adapter-grpc-endpoint http://127.0.0.1:50051 \
  --adapter-grpc-timeout-ms 5000
```

`--adapter-grpc-endpoint` is mutually exclusive with `--adapter-http-endpoint`.

## 7. Python Reference

Reference files:

1. `examples/appfs/grpc-bridge/python/grpc_server.py`
2. `examples/appfs/grpc-bridge/python/http_gateway.py`
3. `examples/appfs/grpc-bridge/python/README.md`

`http_gateway.py` exposes:

1. `POST /v1/submit-action`
2. `POST /v1/submit-control-action`

and forwards to gRPC backend.
