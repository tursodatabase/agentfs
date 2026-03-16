# AppFS Adapter v0.1 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a minimal but Core-compatible AppFS adapter runtime that can pass live AppFS contract tests on Linux while preserving static contract behavior, then extract a reusable Adapter SDK abstraction for multi-language implementations.

**Architecture:** Use a runtime-side adapter loop (`agentfs serve appfs`) that watches `.act` submissions under a mounted AppFS tree, validates payload boundaries (`write+close` semantics at runtime), dispatches to app handlers, and publishes events to `_stream/events.evt.jsonl` with replay/cursor consistency updates. Keep implementation language-neutral at protocol level and Rust-native in AgentFS CLI for Phase 1.

**Tech Stack:** Rust (`tokio`, `serde_json`, `chrono`, `uuid`), shell contract tests (`cli/tests/appfs/*`), existing AgentFS mount flow.

---

## Scope and Non-Goals

1. Scope: Linux-first adapter runtime, Core profile behavior, live contract pass path.
2. Non-goal: Full cross-platform watcher abstraction in Phase 1.
3. Non-goal: Full business adapter marketplace/distribution.
4. Non-goal: Complex search/recommendation semantics beyond contract-level mocks.

## Milestones

1. M1: Runtime command skeleton and lifecycle controls.
2. M2: Action ingestion and event publication.
3. M3: Cursor/replay consistency and paging error mapping.
4. M4: Live contract test harness + docs.
5. M5: Adapter SDK abstraction + v0.1 interface freeze.
6. M6: CI contract gate (static + live) on Linux.

## Progress Snapshot (`2026-03-16`)

1. Completed:
1. Task 1-8 are implemented and validated with live contract coverage (`CT-001` to `CT-016`).
2. Runtime covers long-handle normalization and restart reconciliation for in-flight streaming requests.
3. Task 9 core refactor is implemented: runtime action path and paging control path now dispatch through a frozen SDK trait (`AppAdapterV1`) in `sdk/rust/src/appfs_adapter.rs`.
4. SDK module now includes reusable matrix-style conformance tests (required-case matrix + error-case matrix), with multiple adapter implementations plugged into the same fixture runner.
5. Demo adapter behavior has been split out of `cli/src/cmd/appfs.rs` into a reusable SDK module (`sdk/rust/src/appfs_demo_adapter.rs`) to keep runtime orchestration separate from business logic.
6. Requirements checklist item 1-17 are marked `PASS` in `APPFS-adapter-requirements-v0.1.md`.
7. Optional language bridge reference is now available via HTTP adapter mode in runtime (`--adapter-http-endpoint`), with mapping documented in `APPFS-adapter-http-bridge-v0.1.md` and Python sample at `examples/appfs/http-bridge/python/`.
8. Second transport example (gRPC bridge) is now documented and sampled with proto + Python gRPC service + HTTP gateway at `examples/appfs/grpc-bridge/`.
9. Runtime native gRPC bridge mode is implemented (`--adapter-grpc-endpoint`, `--adapter-grpc-timeout-ms`) for direct out-of-process adapter dispatch.
10. SDK now exposes reusable adapter matrix runners (`sdk/rust/src/appfs_adapter_testkit.rs`) so adapter authors can validate implementations against the frozen trait contract without runtime internals.
11. CI now enforces AppFS contract gates via `.github/workflows/rust.yml` (`appfs-contract-gate`), including static fixture checks and Linux live harness checks.
12. CI now validates out-of-process parity with bridge-mode jobs (`appfs-contract-gate-http-bridge`, `appfs-contract-gate-grpc-bridge`) using the same live suite.
2. Remaining:
1. Optional: add bridge resilience policies (retry/backoff/circuit-breaker) and explicit transport-level observability metrics.

## Task 1: Add `serve appfs` Command Skeleton

**Files:**
- Create: `cli/src/cmd/appfs.rs`
- Modify: `cli/src/cmd/mod.rs`
- Modify: `cli/src/opts.rs`
- Modify: `cli/src/main.rs`
- Test: `cli/tests/appfs/README.md` (usage section)

**Step 1: Write failing smoke assertion**

Run:

```bash
cd cli
cargo run -- serve appfs --help
```

Expected (before implementation): unknown subcommand or no handler.

**Step 2: Add CLI wiring**

Implement `ServeCommand::Appfs { root, app_id, session_id, poll_ms }` and route to `handle_appfs_adapter_command(...)`.

**Step 3: Add minimal runtime loop**

Runtime starts, validates required nodes:

1. `/<root>/<app_id>/_stream/events.evt.jsonl`
2. `/<root>/<app_id>/_stream/cursor.res.json`
3. `/<root>/<app_id>/_stream/from-seq`

Process blocks until `Ctrl+C`.

**Step 4: Verify command works**

Run:

```bash
cd cli
cargo run -- serve appfs --root /tmp/app --app-id aiim
```

Expected: startup log + waiting loop (or clear readiness error).

**Step 5: Commit**

```bash
git add cli/src/cmd/appfs.rs cli/src/cmd/mod.rs cli/src/opts.rs cli/src/main.rs
git commit -m "feat(appfs): add serve appfs command skeleton"
```

## Task 2: Implement Action Discovery and Submit Boundary Handling

**Files:**
- Modify: `cli/src/cmd/appfs.rs`
- Test: `cli/tests/appfs/test-action-basics.sh`

**Step 1: Establish failing baseline**

Run existing live contract (without adapter behavior):

```bash
cd cli
APPFS_CONTRACT_TESTS=1 APPFS_ROOT=/path/to/mounted/app ./tests/test-appfs-contract.sh
```

Expected: `CT-002` fails on stream growth.

**Step 2: Implement `.act` detection**

1. Track `*.act` files under `/app/<app_id>`.
2. Treat modifications as submit candidates.
3. Ignore incomplete/empty payloads.
4. Ensure commit boundary aligns with file close observation (polling + stable fingerprint).

**Step 3: Enforce sink behavior for test compatibility**

After accepted submission:

1. Make the action sink non-readable/non-appendable for the same request window.
2. Keep behavior deterministic for contract checks (`cat` and `>>` fail in `CT-002`).

**Step 4: Re-run targeted live test**

Expected: `CT-002` advances past event-growth and sink-operation checks.

**Step 5: Commit**

```bash
git add cli/src/cmd/appfs.rs
git commit -m "feat(appfs): detect .act submissions and enforce sink semantics"
```

## Task 3: Emit Core Event Envelope with Stable IDs

**Files:**
- Modify: `cli/src/cmd/appfs.rs`
- Reference: `APPFS-v0.1.md`, `APPFS-adapter-requirements-v0.1.md`

**Step 1: Add event model**

Each emitted line includes:

1. `seq`
2. `event_id`
3. `ts`
4. `app`
5. `session_id`
6. `request_id`
7. `path`
8. `type`

**Step 2: Add request correlation extraction**

1. JSON payload `client_token`
2. Text payload first-line `token:<value>`

**Step 3: Emit deterministic event types**

1. `send_message.act` -> `action.completed`
2. `fetch_next.act` -> `action.completed` with page envelope
3. `close.act` -> terminal outcome with mapped result

**Step 4: Verify event schema by jq**

Run:

```bash
tail -n 1 /path/to/events.evt.jsonl | jq -e '.event_id and .request_id and .type'
```

Expected: PASS.

**Step 5: Commit**

```bash
git add cli/src/cmd/appfs.rs
git commit -m "feat(appfs): emit core event envelope with stable event_id"
```

## Task 4: Implement Cursor/Replay Atomic Update Path

**Files:**
- Modify: `cli/src/cmd/appfs.rs`
- Test: `cli/tests/appfs/test-stream-replay.sh`

**Step 1: Add transactional publish sequence**

For each event `N`:

1. Append event line
2. Materialize `/from-seq/N.evt.jsonl`
3. Update `cursor.max_seq`

Use temp-file + atomic rename for `cursor.res.json`.

**Step 2: Crash-safety behavior**

Ensure no state where `cursor.max_seq` points past durable stream lines.

**Step 3: Re-run replay checks**

```bash
cd cli
APPFS_CONTRACT_TESTS=1 APPFS_ROOT=/path/to/mounted/app ./tests/test-appfs-contract.sh
```

Expected: `CT-003` remains green.

**Step 4: Commit**

```bash
git add cli/src/cmd/appfs.rs
git commit -m "feat(appfs): keep events/cursor/from-seq consistent on publish"
```

## Task 5: Implement Paging Error Mapping Contract

**Files:**
- Modify: `cli/src/cmd/appfs.rs`
- Test: `cli/tests/appfs/test-paging.sh`

**Step 1: Add paging handle validator**

Map errors:

1. malformed -> `EINVAL` (no accept event)
2. unknown -> `PAGER_HANDLE_NOT_FOUND`
3. expired -> `PAGER_HANDLE_EXPIRED`
4. closed -> `PAGER_HANDLE_CLOSED`
5. cross-session -> `PERMISSION_DENIED`

**Step 2: Verify CT-004**

Expected: stream growth after `fetch_next` and deterministic terminal behavior.

**Step 3: Commit**

```bash
git add cli/src/cmd/appfs.rs
git commit -m "feat(appfs): add paging handle error mapping"
```

## Task 6: Add Live Harness Script for Repeatable Validation

**Files:**
- Create: `cli/tests/appfs/run-live-with-adapter.sh`
- Modify: `cli/tests/appfs/README.md`

**Step 1: Script responsibilities**

1. Mount AgentFS
2. Copy AppFS fixture
3. Start `agentfs serve appfs`
4. Run `APPFS_CONTRACT_TESTS=1` live suite
5. Teardown mount + adapter process

**Step 2: Verify from clean state**

Run:

```bash
cd cli
./tests/appfs/run-live-with-adapter.sh
```

Expected: static tests pass; live tests pass for implemented scope.

**Step 3: Commit**

```bash
git add cli/tests/appfs/run-live-with-adapter.sh cli/tests/appfs/README.md
git commit -m "test(appfs): add live adapter contract harness"
```

## Task 7: Documentation and Conformance Declaration

**Files:**
- Modify: `APPFS-v0.1.md`
- Modify: `APPFS-adapter-requirements-v0.1.md`
- Modify: `APPFS-conformance-v0.1.md`
- Modify: `examples/appfs/aiim/_meta/manifest.res.json`

**Step 1: Add conformance metadata sample**

Include:

1. `conformance.profiles`
2. `implementation.name/version/language`
3. extension declaration pattern

**Step 2: Add "how to claim compatibility" section to README/docs**

Expected: third-party implementers can self-check Core compatibility.

**Step 3: Commit**

```bash
git add APPFS-v0.1.md APPFS-adapter-requirements-v0.1.md APPFS-conformance-v0.1.md examples/appfs/aiim/_meta/manifest.res.json
git commit -m "docs(appfs): publish conformance declaration examples"
```

## Task 8: Final Validation and Release Checklist

**Files:**
- Modify: `APPFS-adapter-requirements-v0.1.md` (checkboxes if needed)

**Step 1: Build + tests**

```bash
cd cli
cargo build
APPFS_CONTRACT_TESTS=1 APPFS_STATIC_FIXTURE=1 ./tests/test-appfs-contract.sh
APPFS_CONTRACT_TESTS=1 APPFS_ROOT=/path/to/mounted/app ./tests/test-appfs-contract.sh
```

**Step 2: Verify acceptance checklist items 1-16**

Mark each as pass/fail with evidence link (command output or log path).

**Step 3: Final commit**

```bash
git add -A
git commit -m "feat(appfs): phase1 adapter runtime and conformance-ready contract pass"
```

## Task 9: Extract Adapter SDK Abstraction and Freeze v0.1 Interface

**Files:**
- Modify: `cli/src/cmd/appfs.rs`
- Create: `sdk/rust/src/appfs_adapter.rs` (or equivalent module)
- Modify: `sdk/rust/src/lib.rs`
- Modify: `APPFS-adapter-requirements-v0.1.md`
- Modify: `APPFS-conformance-v0.1.md`

**Step 1: Define frozen v0.1 adapter contract**

1. Introduce a versioned trait surface (example: `AppAdapterV1`, `EventEmitterV1`, `RequestContextV1`).
2. Move current "suggested interface" into explicit v0.1 frozen contract text.
3. Define allowed change policy:
1. `v0.1.x`: additive-only (non-breaking).
2. breaking changes deferred to `v0.2`.

**Step 2: Split runtime from demo adapter behavior**

1. Keep runtime responsible for file watching, request-id assignment, stream persistence, and replay/cursor consistency.
2. Move app-specific action/resource behavior behind adapter trait calls.
3. Keep existing aiim demo logic as one implementation of the trait.

**Step 3: Add SDK-level conformance harness entry points**

1. Add adapter-focused tests so different implementations can be plugged in without changing runtime code.
2. Verify existing live suite still passes after refactor.

**Step 4: Publish compatibility guidance for third-party developers**

1. Clarify that any language can implement the adapter, as long as AppFS contract semantics are preserved.
2. Add mapping notes (Rust trait vs gRPC/HTTP bridge equivalents).
3. Add "how to claim adapter compatibility" checklist with required test evidence.

**Step 5: Commit**

```bash
git add cli/src/cmd/appfs.rs sdk/rust/src/appfs_adapter.rs sdk/rust/src/lib.rs APPFS-adapter-requirements-v0.1.md APPFS-conformance-v0.1.md
git commit -m "refactor(appfs): extract adapter sdk abstraction and freeze v0.1 interface"
```

## Risks and Mitigations

1. File polling latency can cause flaky live tests.
Mitigation: configurable `poll_ms`, wait-for-growth utility, bounded backoff.

2. Action sink semantics (`cat`/`append`) can differ by filesystem backend.
Mitigation: enforce sink state immediately after submit and validate under target backend.

3. Cursor/replay consistency under crash.
Mitigation: atomic cursor update and publish ordering tests.

## Definition of Done (Phase 1)

1. `agentfs serve appfs` available and documented.
2. Live contract path (`CT-001` to `CT-016`) is passing on Linux with adapter runtime enabled.
3. Event envelope includes stable `event_id`.
4. Paging error mapping follows documented contract.
5. Conformance declaration path is documented for third-party implementations.

## Definition of Done (Phase 1.5: SDK Freeze)

1. Runtime and adapter business logic are decoupled through a versioned adapter interface.
2. v0.1 adapter contract is explicitly frozen and documented.
3. Third-party adapter authors can implement against the contract without reading runtime internals.
4. Existing contract tests still pass after abstraction extraction.


