# AppFS v0.1 Contract Test Plan

- Version: `0.1-draft-r8`
- Date: `2026-03-16`
- Status: `Draft`
- Depends on: `APPFS-v0.1 (r7)`, `APPFS-adapter-requirements-v0.1`

## 1. Purpose

This plan defines executable contract checks for AppFS v0.1.

Goals:

1. Convert spec MUST clauses into repeatable tests.
2. Provide a stable gate for runtime and adapter changes.
3. Keep tests shell-first to match LLM+bash usage.

## 2. Test Entry

Runner:

```bash
cd cli
APPFS_CONTRACT_TESTS=1 ./tests/test-appfs-contract.sh
```

Static fixture mode (no live runtime):

```bash
cd cli
APPFS_CONTRACT_TESTS=1 APPFS_STATIC_FIXTURE=1 APPFS_ROOT=/mnt/c/Users/esp3j/rep/agentfs/examples/appfs ./tests/test-appfs-contract.sh
```

Optional aggregate runner:

```bash
cd cli
APPFS_CONTRACT_TESTS=1 ./tests/all.sh
```

Linux CI gate (GitHub Actions):

1. Static fixture gate:

```bash
APPFS_CONTRACT_TESTS=1 APPFS_STATIC_FIXTURE=1 APPFS_ROOT=$GITHUB_WORKSPACE/examples/appfs sh ./tests/test-appfs-contract.sh
```

2. Live mount + adapter gate:

```bash
APPFS_CONTRACT_TESTS=1 sh ./tests/appfs/run-live-with-adapter.sh
```

## 3. Environment Inputs

| Variable | Default | Description |
|---|---|---|
| `APPFS_CONTRACT_TESTS` | `0` | Set `1` to enable AppFS contract tests |
| `APPFS_ROOT` | `/app` | Mounted AppFS root |
| `APPFS_APP_ID` | `aiim` | App id under `/app` |
| `APPFS_TEST_ACTION` | `/app/aiim/contacts/zhangsan/send_message.act` | Action sink used by action tests |
| `APPFS_PAGEABLE_RESOURCE` | `/app/aiim/chats/chat-001/messages.res.json` | Pageable resource used by paging tests |
| `APPFS_TIMEOUT_SEC` | `10` | Wait timeout for async assertions |
| `APPFS_STATIC_FIXTURE` | `0` | Set `1` to run only static checks against fixture trees |

## 4. Contract Suite

Note: `cli/tests/appfs/` currently contains CT-001 through CT-016. Sections below highlight baseline CT-001~CT-005 and the same runner also executes extended live checks (`CT-006` streaming lifecycle, `CT-007` close-time reject behavior, `CT-008` submit ordering, `CT-009` paging error mapping, `CT-010`/`CT-011` submit atomicity/interruption, `CT-012` path safety, `CT-013` duplicate consumption, `CT-014` concurrent submit stress, `CT-015` long-handle normalization, `CT-016` restart reconciliation).

### CT-001 Layout and Required Nodes

Spec refs:

1. `APPFS-v0.1` section 4.
2. `APPFS-v0.1` section 13.

Assertions:

1. Required files exist (`manifest`, `context`, `permissions`, `events`, `cursor`, `from-seq`, `_paging/fetch_next.act`, `_paging/close.act`).
2. `manifest` has `app_id` and `nodes`.

Script:

```text
cli/tests/appfs/test-layout.sh
```

### CT-002 Action Sink Semantics

Spec refs:

1. `APPFS-v0.1` section 7.
2. `APPFS-v0.1` section 8.

Assertions:

1. `write+close` to `.act` succeeds.
2. Event stream grows after action submission.
3. `.act` read (`cat`) fails.
4. `.act` append (`>>`) fails.
5. New terminal event contains `request_id` and `type` (when `jq` is available).

Script:

```text
cli/tests/appfs/test-action-basics.sh
```

### CT-003 Stream Replay and Cursor

Spec refs:

1. `APPFS-v0.1` section 8 (replay/resume).

Assertions:

1. `cursor.res.json` has `min_seq`, `max_seq`, `retention_hint_sec`.
2. `from-seq/<seq>.evt.jsonl` returns data for valid sequence.
3. `from-seq/<min_seq-1>.evt.jsonl` fails when older than retained window.

Script:

```text
cli/tests/appfs/test-stream-replay.sh
```

### CT-004 Paging Handle Protocol

Spec refs:

1. `APPFS-v0.1` section 11.

Assertions:

1. Pageable `cat` returns `{items, page}`.
2. `page.handle_id` is present.
3. `fetch_next.act` accepts `handle_id`.
4. Stream contains completion event for paging action.
5. `close.act` accepts `handle_id`.

Script:

```text
cli/tests/appfs/test-paging.sh
```

### CT-005 Manifest Policy Checks

Spec refs:

1. `APPFS-v0.1` section 5.
2. `APPFS-v0.1` section 13.

Assertions:

1. Node names do not contain forbidden path patterns (`..`, backslash, drive letters).
2. Action nodes define expected fields (`input_mode`, `execution_mode`).
3. Pageable resources define `paging` metadata.

Script:

```text
cli/tests/appfs/test-manifest-policy.sh
```

## 5. Gaps and Follow-up (v0.2 Candidate)

Not fully covered by shell black-box tests today:

1. Runtime pre-router unsafe segment rejection before backend side effects (needs lower-layer API test hooks).
2. `at-least-once` duplicate delivery behavior under crash/retry simulation.
3. Segment shortening hash determinism for generated IDs across adapters.

Recommended follow-up:

1. Add SDK-level and unit-level tests in runtime crate for path normalization and guard ordering.
2. Add fault-injection test harness for stream durability and replay.
