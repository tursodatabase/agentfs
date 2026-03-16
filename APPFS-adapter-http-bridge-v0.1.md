# AppFS Adapter HTTP Bridge v0.1 (Reference)

- Version: `0.1-draft`
- Date: `2026-03-16`
- Status: `Draft`
- Scope: Optional language bridge for `AppAdapterV1`

## 1. Purpose

This document defines a minimal HTTP mapping so non-Rust adapters can integrate with `agentfs serve appfs` while preserving AppFS Core semantics.

## 2. Runtime Switch

When starting runtime:

```bash
agentfs serve appfs \
  --root /app \
  --app-id aiim \
  --adapter-http-endpoint http://127.0.0.1:8080 \
  --adapter-http-timeout-ms 5000
```

Equivalent env vars:

1. `APPFS_ADAPTER_HTTP_ENDPOINT`
2. `APPFS_ADAPTER_HTTP_TIMEOUT_MS`

If endpoint is omitted, runtime uses in-process `DemoAppAdapterV1`.

## 3. Endpoints

HTTP `POST` JSON endpoints expected by runtime:

1. `/v1/submit-action`
2. `/v1/submit-control-action`

## 4. Request/Response Shapes

### 4.1 `POST /v1/submit-action`

Request:

```json
{
  "app_id": "aiim",
  "path": "/contacts/zhangsan/send_message.act",
  "payload": "hello\n",
  "input_mode": "text",
  "execution_mode": "inline",
  "context": {
    "app_id": "aiim",
    "session_id": "sess-1234",
    "request_id": "req-5678",
    "client_token": "tok-1"
  }
}
```

Success response MUST be `AdapterSubmitOutcomeV1` JSON:

```json
{
  "kind": "completed",
  "content": "send success"
}
```

or:

```json
{
  "kind": "streaming",
  "plan": {
    "accepted_content": "accepted",
    "progress_content": { "percent": 50 },
    "terminal_content": { "ok": true }
  }
}
```

### 4.2 `POST /v1/submit-control-action`

Request:

```json
{
  "app_id": "aiim",
  "path": "/_paging/fetch_next.act",
  "action": {
    "kind": "paging_fetch_next",
    "handle_id": "ph_abc",
    "page_no": 1,
    "has_more": true
  },
  "context": {
    "app_id": "aiim",
    "session_id": "sess-1234",
    "request_id": "req-5678",
    "client_token": "tok-1"
  }
}
```

Success response MUST be `AdapterControlOutcomeV1` JSON:

```json
{
  "kind": "completed",
  "content": {
    "closed": true
  }
}
```

## 5. Error Mapping

Bridge service SHOULD return one of:

1. `AdapterErrorV1` JSON (preferred)
2. Simple error JSON:

```json
{
  "code": "PERMISSION_DENIED",
  "message": "forbidden",
  "retryable": false
}
```

Runtime behavior:

1. First parse full `AdapterErrorV1`.
2. Fallback parse simple `{code,message,retryable}`.
3. Otherwise map to `AdapterErrorV1::Internal` with HTTP status/body.

## 6. Compatibility Note

This bridge only changes transport. AppFS protocol guarantees still stay in runtime:

1. write+close submit boundary
2. event persistence/order
3. cursor/replay atomicity
4. paging close-time error semantics
