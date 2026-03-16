# AppFS Adapter Layer Requirements v0.1

- Version: `0.1-draft-r3`
- Date: `2026-03-16`
- Status: `Draft`
- Depends on: `APPFS-v0.1 (r8)`
- Conformance profile: `APPFS-conformance-v0.1.md`

## 1. Decision

Current AppFS v0.1 design is sufficient to start adapter implementation.

Reason:

1. Core interaction loop is closed: `.act` write -> stream events.
2. Action modes are defined: `inline` and `streaming`.
3. Discovery contract exists: `_meta/manifest.res.json` + schemas.
4. Replay baseline exists: `cursor` + `from-seq`.

Known non-blocking gaps remain for later versions (multi-tenant sharing, unified cancel spec, QoS classes).

## 2. Scope

This document defines requirements for the adapter layer only.

In scope:

1. Mapping AppFS nodes to real app operations.
2. Action execution and event emission.
3. Schema and capability publication.
4. Validation and error mapping.

Out of scope:

1. Mount backend implementation (FUSE/WinFsp/NFS).
2. Generic filesystem metadata internals.
3. Cross-app orchestration/transactions.

## 3. Roles and Boundaries

### 3.1 Runtime Responsibilities

1. Path routing and filesystem operation dispatch.
2. Session/principal context injection.
3. Request ID generation (server-side).
4. Stream storage and replay surface (`events`, `cursor`, `from-seq`).
5. Path normalization and unsafe-path precheck before calling adapters.

### 3.2 Adapter Responsibilities

1. Domain/resource/action registration.
2. Resource read realization.
3. Action payload validation and execution.
4. Event production according to AppFS schema.
5. App-specific permission checks and policy enforcement.

## 4. Functional Requirements

### AR-001 Manifest Publication

Adapter MUST provide data required to produce `_meta/manifest.res.json`:

1. Node list and kind (`resource`/`action`).
2. `input_mode`, `execution_mode`, schema references.
3. Action limits (`max_payload_bytes`, optional `rate_limit_hint`).

### AR-002 Resource Read

1. Adapter MUST resolve `*.res.json` nodes to UTF-8 JSON output.
2. Missing resource MUST map to `ENOENT`.
3. Unauthorized resource MUST map to `EACCES`.

### AR-003 Action Submit (`*.act`)

1. Runtime calls adapter on `write+close`.
2. Adapter MUST validate payload according to `input_mode` and declared schema.
3. Validation failure MUST return a deterministic error (`EINVAL`/`EMSGSIZE`) and MUST NOT emit `action.accepted`.
4. Accepted requests MUST produce stream events with runtime-provided `request_id`.
5. Runtime MUST treat `close` as the only submission boundary. Partial or interrupted writes before `close` MUST be discarded and MUST NOT trigger side effects.
6. Runtime SHOULD stage request bytes and atomically promote them to submission input so adapter never receives truncated payload.

### AR-004 Execution Modes

#### AR-004A Inline Mode

1. Adapter SHOULD complete within `inline_timeout_ms`.
2. Adapter MAY return synchronous success/failure.
3. Adapter SHOULD emit terminal event (`action.completed` or `action.failed`) even when handled synchronously.
4. If timeout exceeded, adapter MAY degrade to async and emit `action.accepted`.

#### AR-004B Streaming Mode

1. Adapter MUST emit `action.accepted` quickly after submission.
2. Adapter SHOULD emit `action.progress` at app-defined cadence.
3. Adapter MUST emit exactly one terminal event (`action.completed` or `action.failed`, optionally `action.canceled`).

### AR-005 Event Contract

Each emitted event line MUST include:

1. `seq` (assigned by runtime stream layer)
2. `event_id`
3. `ts`
4. `app`
5. `session_id`
6. `request_id`
7. `path`
8. `type`

For `action.failed`, `error.code` and `error.message` MUST be present.
`event_id` MUST be stable across replay and unique within the app stream retention window.

### AR-006 Correlation

1. Adapter MUST support server-generated `request_id`.
2. If payload contains `client_token` (or text-mode `token:` prefix), adapter SHOULD echo it in event payload for correlation.

### AR-007 Replay Support Cooperation

1. Adapter MUST emit events in causal order per request.
2. Adapter MUST tolerate replay readers reconnecting from older `seq` values exposed by runtime.

### AR-008 Search Support

1. Adapter SHOULD provide simple projection resources (`by-name/.../index.res.json`) where applicable.
2. Adapter MAY expose complex search action sinks (`search.act`) with cursorized outputs in events.

### AR-009 Error Mapping

Adapter MUST map app errors to:

1. Filesystem errno class.
2. Structured event error payload (`code`, `message`, optional `retryable`, `details`).

### AR-010 Path Safety Guard

1. Runtime+adapter chain MUST reject traversal-style or unsafe paths before side effects.
2. Rejected cases include at least: `.`/`..` segments, drive-letter injection (`C:`), backslash-separated traversal, and NUL bytes.
3. On unsafe input, adapter-facing business handlers MUST NOT run (no app/backend side effect).

### AR-011 Filename/ID Portability Guard

1. Adapter MUST enforce AppFS segment character policy and reserved-name policy.
2. For runtime-generated segments exceeding 255 UTF-8 bytes, adapter MUST apply deterministic shortening with hash suffix.
3. The same input MUST produce the same shortened output.

### AR-012 Stream Delivery Semantics

1. Event delivery contract is `at-least-once`.
2. Adapter MUST assume replay and duplicate-consumption scenarios are normal.
3. Runtime MUST assign stable `event_id`; adapter and replay layers MUST preserve it unchanged.
4. Adapter-emitted payload SHOULD include stable correlation hints (`request_id`, optional `client_token`).

### AR-013 Observer Publication

Adapter SHOULD expose or feed data for `/app/<app_id>/_meta/observer.res.json`:

1. action counters (`accepted_total`, `completed_total`, `failed_total`)
2. latency aggregates (`p95_accept_ms`, `p95_end_to_end_ms`)
3. stream pressure (`stream_backlog`)
4. last error timestamp (`last_error_ts`)

### AR-014 Paging Handle Error Contract

`/_paging/fetch_next.act` and `/_paging/close.act` MUST follow deterministic error mapping:

1. Malformed `handle_id` format MUST fail at close-time with `EINVAL` and MUST NOT emit `action.accepted`.
2. Unknown handle MUST emit `action.failed` with `error.code = "PAGER_HANDLE_NOT_FOUND"`.
3. Expired handle MUST emit `action.failed` with `error.code = "PAGER_HANDLE_EXPIRED"`.
4. Already-closed handle MUST emit `action.failed` with `error.code = "PAGER_HANDLE_CLOSED"`.
5. Cross-session handle access MUST emit `action.failed` with `error.code = "PERMISSION_DENIED"` (optionally with app code detail).

### AR-015 Concurrent Submit Ordering

1. Runtime MUST preserve causal ordering within each `request_id`.
2. For the same `(app_id, session_id, action_path)`, `action.accepted` sequence MUST follow `close` commit order.
3. Exactly one terminal event MUST be emitted per accepted request even under concurrent submissions.

### AR-016 Stream Surface Atomicity

For each committed event `seq = N`, runtime MUST atomically keep these surfaces consistent:

1. `_stream/events.evt.jsonl` contains the event line for `N`.
2. `_stream/cursor.res.json` has `max_seq >= N`.
3. `_stream/from-seq/N.evt.jsonl` is readable and includes `seq >= N`.

Crash/restart MUST not expose partially published state where `cursor` points past durable event data.

### AR-017 Adapter Lifecycle and Health

1. Adapter runtime MUST expose readiness before accepting `.act` submissions.
2. Adapter runtime MUST expose liveness/health status (direct endpoint or reflected observer metrics).
3. On graceful shutdown, adapter runtime MUST stop new accepts first, then drain or mark in-flight requests deterministically.
4. On restart recovery, runtime MUST reconcile accepted-but-not-terminal requests and emit deterministic terminal outcomes.

### AR-018 Adapter SDK Abstraction and Interface Freeze

1. Runtime MUST invoke business adapter logic through a stable adapter SDK contract rather than hard-coded demo branches.
2. AppFS Adapter SDK v0.1 interface MUST be explicitly frozen (`v0.1.x` additive-only; breaking changes require `v0.2`).
3. Third-party implementations in any language are allowed, but they MUST preserve AppFS protocol semantics and pass conformance tests.
4. Conformance docs MUST publish the frozen method-level contract and compatibility claim criteria.

## 5. Non-Functional Requirements

### ANR-001 Latency

1. `inline` actions target: P95 <= 2s (app-dependent).
2. `streaming` acceptance target: P95 <= 1s to `action.accepted`.

### ANR-002 Reliability

1. After `action.accepted`, terminal event MUST eventually appear unless process crash occurs.
2. Adapter SHOULD be crash-safe by delegating durable stream persistence to runtime.
3. Recovery path MUST preserve event ordering per request.

### ANR-003 Observability

Adapter MUST expose structured logs including:

1. `request_id`
2. action path
3. execution mode
4. latency
5. result status
6. normalized error code (when failed)

## 6. Adapter SDK Interface (Rust-Oriented, v0.1 Frozen Surface)

The following trait shape is the v0.1 frozen logical contract (language-neutral at protocol semantics, Rust shown as reference).

```rust
pub trait AppAdapterV1: Send {
    fn app_id(&self) -> &str;

    fn submit_action(
        &mut self,
        path: &str,
        payload: &str,
        input_mode: AdapterInputModeV1,
        execution_mode: AdapterExecutionModeV1,
        ctx: &RequestContextV1,
    ) -> Result<AdapterSubmitOutcomeV1, AdapterErrorV1>;

    fn submit_control_action(
        &mut self,
        path: &str,
        action: AdapterControlActionV1,
        ctx: &RequestContextV1,
    ) -> Result<AdapterControlOutcomeV1, AdapterErrorV1>;
}
```

Where:

1. `RequestContextV1` is runtime-provided correlation context (`app_id`, `session_id`, `request_id`, optional `client_token`).
2. `submit_action` returns either:
1. `AdapterSubmitOutcomeV1::Completed` (inline-style terminal content)
2. `AdapterSubmitOutcomeV1::Streaming` (accepted/progress/terminal plan for runtime emission)
3. `submit_control_action` is for control channels (currently paging `fetch_next` / `close`).
4. Runtime keeps ownership of stream durability (`events`, `cursor`, `from-seq`) and ordering/atomicity guarantees.

### 6.1 v0.1 Freeze Policy

1. `v0.1.x` only allows backward-compatible additive changes.
2. Removing/renaming/changing behavior of existing required methods is a breaking change and MUST wait for `v0.2`.
3. Any implementation-specific extension MUST be documented as optional and MUST NOT alter Core semantics.
4. Manifest SHOULD expose adapter compatibility metadata (e.g., `adapter_sdk_version`).

### 6.2 Conformance Fixture Guidance

1. SDK SHOULD provide reusable fixture-style tests for:
1. required submit/control case matrix
2. error case matrix
2. Different adapter implementations SHOULD be pluggable into the same fixture matrix without changing runtime logic.
3. Current Rust SDK reference includes matrix-style trait tests in `sdk/rust/src/appfs_adapter.rs`.
4. Current Rust SDK reference demo implementation is published at `sdk/rust/src/appfs_demo_adapter.rs`.
5. Current CLI runtime includes optional HTTP bridge transport for out-of-process adapters; mapping is documented in `APPFS-adapter-http-bridge-v0.1.md`.

## 7. Security Requirements

1. Adapter MUST consume principal/session context from runtime (from `_meta/context` model).
2. Adapter MUST enforce app-level scopes before side effects.
3. For approval-required actions, adapter MUST emit `action.awaiting_approval` and defer terminal result.

## 8. Validation and Acceptance Checklist

Adapter implementation is accepted when all checks pass:

1. Manifest completeness: node/action/schema fields present.
2. `.act` validation path: malformed payload returns sync error and no `action.accepted`.
3. Inline action path: sync result works; terminal event emitted.
4. Streaming action path: accepted -> progress(optional) -> terminal flow works.
5. Failed action path: `action.failed` contains structured `error`.
6. Correlation: `request_id` always present; `client_token` echoed when provided.
7. Replay compatibility: events can be consumed via `from-seq`.
8. Unsafe path guard: traversal/drive-injection/backslash payloads are rejected before side effects.
9. Segment portability: overlong generated names are deterministically shortened with hash and remain <= 255 bytes.
10. Delivery semantics: consumer-side duplicate handling is validated in integration tests.
11. Paging handle errors: malformed/unknown/expired/closed/cross-session cases map to required error codes.
12. Submit atomicity: interrupted writes do not create requests or events.
13. Concurrent ordering: same action path preserves accept order and single terminal event per request.
14. Stream atomicity: `events`, `cursor`, and `from-seq` stay consistent for every committed `seq`.
15. `event_id` is present on all events and remains stable across replay.
16. Lifecycle checks: readiness/liveness/shutdown/recovery behaviors are verified in integration tests.
17. Adapter SDK freeze: runtime delegates through frozen adapter interface and compatibility policy is documented.

### 8.1 Phase 1 Validation Snapshot (`2026-03-16`)

Evidence sources used:

1. Build + static + live run log: `/home/yxy/rep/agentfs/cli/appfs-phase1-validation.log`
2. Live harness script: `cli/tests/appfs/run-live-with-adapter.sh`
3. Runtime implementation: `cli/src/cmd/appfs.rs`
4. Live contract additions: `cli/tests/appfs/test-streaming-lifecycle.sh`, `cli/tests/appfs/test-submit-reject.sh`, `cli/tests/appfs/test-submit-order.sh`, `cli/tests/appfs/test-paging-errors.sh`, `cli/tests/appfs/test-submit-atomicity.sh`, `cli/tests/appfs/test-submit-interrupt.sh`, `cli/tests/appfs/test-path-safety.sh`, `cli/tests/appfs/test-duplicate-consumption.sh`, `cli/tests/appfs/test-concurrent-submit-stress.sh`
5. Harness lifecycle probe: `cli/tests/appfs/run-live-with-adapter.sh` (stop/restart adapter + post-restart submit)
6. SDK matrix fixture tests: `sdk/rust/src/appfs_adapter.rs` (`sdk_trait_required_case_matrix_is_adapter_pluggable`, `sdk_trait_error_case_matrix`)

| Item | Status | Evidence | Note |
|---|---|---|---|
| 1 | PASS | `CT-001/CT-005` in validation log | Manifest nodes and schemas present |
| 2 | PASS | `CT-007` in validation log | Malformed JSON and malformed handle are rejected without `action.accepted` |
| 3 | PASS | `CT-002` in validation log | Inline action emits terminal event |
| 4 | PASS | `CT-006` in validation log | Streaming action emits `accepted/progress/completed` with single terminal |
| 5 | PASS | `CT-004` + `emit_failed` code path | `action.failed.error` structure emitted |
| 6 | PASS | `CT-002` + token extraction logic | `request_id` always present; `client_token` echo supported |
| 7 | PASS | `CT-003` in validation log | Replay via `from-seq` works |
| 8 | PASS | `CT-012` in validation log + `cli/src/cmd/appfs.rs` (`is_safe_action_rel_path`) | drive-letter/reserved/backslash unsafe paths are rejected without stream side effects |
| 9 | PASS | `CT-015` in validation log + `cli/src/cmd/appfs.rs` (`normalize_runtime_handle_id`) | Overlong paging handles are deterministically shortened to portable <=255-byte runtime IDs while preserving alias lookup |
| 10 | PASS | `CT-013` in validation log | Same event is consumable from both live stream and replay surface; consumer dedupe is required |
| 11 | PASS | `CT-009` in validation log + `cli/src/cmd/appfs.rs` | malformed/unknown/expired/closed/cross-session paging errors are mapped and asserted |
| 12 | PASS | `CT-010/CT-011` in validation log + `cli/src/cmd/appfs.rs` stable-submit gate | In-progress and interrupted write scenarios are covered; no side effect before valid completed submit |
| 13 | PASS | `CT-014` in validation log | Concurrent submit stress validates single terminal per submit and distinct request ids |
| 14 | PASS | `CT-003` + publish sequence in code | `events/cursor/from-seq` consistency validated for normal publish path |
| 15 | PASS | `CT-002/CT-003` + seq-based `event_id` | `event_id` present and replay-stable |
| 16 | PASS | `CT-016` in `run-live-with-adapter.sh` validation log + `cli/src/cmd/appfs.rs` (`inflight.jobs.res.json`) | graceful stop/restart and accepted-but-not-terminal streaming reconciliation are validated end-to-end |
| 17 | PASS | `sdk/rust/src/appfs_adapter.rs` + `cli/src/cmd/appfs.rs` + `run-live-with-adapter.sh` (`CT-001` to `CT-016`) | Runtime dispatches business action handling through the frozen `AppAdapterV1` contract and preserves live conformance behavior |

## 9. Delivery Plan

### Phase 1 (Core Adapter Skeleton)

1. Manifest generation.
2. Resource read handlers.
3. Action submit pipeline with validation.

### Phase 2 (Mode Semantics)

1. Inline mode behavior and timeout fallback.
2. Streaming mode progress and terminal guarantees.

### Phase 3 (Hardening)

1. Error mapping consistency.
2. Permission checks and approval flow.
3. Contract tests and performance baselines.
