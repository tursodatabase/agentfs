# AppFS Conformance Profile v0.1

- Version: `0.1`
- Date: `2026-03-16`
- Status: `Draft`
- Depends on:
  - `APPFS-v0.1 (r8)`
  - `APPFS-adapter-requirements-v0.1 (r2)`

## 1. Purpose

This document defines how an implementation claims AppFS compatibility.

It is language-neutral and runtime-neutral:

1. Any implementation language is allowed.
2. Any deployment shape is allowed (in-process, sidecar, daemon, service).
3. Compatibility is judged by external behavior and contract tests, not internal design.

## 2. Compatibility Levels

### 2.1 Core Profile (Required)

An implementation is **AppFS v0.1 Core compatible** only if all Core requirements are met:

1. Required namespace and per-app layout.
2. `.act` write+close semantics.
3. Stream event contract with stable `event_id`.
4. Replay surfaces: `cursor` and `from-seq`.
5. Paging protocol (`fetch_next.act`, `close.act`) and error mapping.
6. Path safety and portability guards.
7. Atomicity and ordering constraints defined in adapter requirements.

### 2.2 Recommended Profile (Optional)

Optional `SHOULD` items from spec can be claimed as **Recommended compatible**.

This includes (example):

1. Observer surface.
2. Progress cadence hints.
3. Extra search projections.

### 2.3 Extension Profile (Optional)

Vendor/app extensions are allowed if:

1. Core behavior remains unchanged.
2. Unknown fields/paths do not break Core clients.
3. Extension keys are namespaced (recommended: `x_<vendor>_*`).

## 3. Version Compatibility Rules

1. `contract_version` MUST be present in manifest.
2. `0.1.x` patch revisions MUST remain backward compatible at Core level.
3. Additive fields are allowed; removals or semantic breaks require a new minor/major contract version.
4. Agents SHOULD treat unknown fields as ignorable unless declared critical by profile.

## 4. Conformance Statement

Each adapter/runtime SHOULD publish a conformance block in manifest metadata:

```json
{
  "conformance": {
    "appfs_version": "0.1",
    "profiles": ["core"],
    "recommended": ["observer", "progress_policy"],
    "extensions": ["x_example_batch_send"],
    "implementation": {
      "name": "aiim-adapter",
      "version": "0.1.0",
      "language": "rust"
    }
  }
}
```

`language` is informational only and does not affect compatibility.

## 5. Test-Based Compliance

### 5.1 Minimum Test Gates

To claim Core compatibility, implementation MUST pass:

1. AppFS static contract tests (`CT-001`, `CT-003`, `CT-005`).
2. AppFS live contract suite (`CT-002`, `CT-004`, `CT-006` to `CT-016`).
3. Adapter acceptance checklist items in `APPFS-adapter-requirements-v0.1`.
4. CI gate MUST include both static and live contract execution (reference: `.github/workflows/rust.yml`, job `appfs-contract-gate`).

### 5.2 Failure Policy

1. Any Core MUST violation means **not Core compatible**.
2. Recommended/Extension failures do not invalidate Core claim.

## 6. Interoperability Rules for Agents

Agent clients SHOULD:

1. Check `contract_version` and `conformance.profiles`.
2. Proceed if `core` is present and required nodes exist.
3. Ignore unknown extension fields.
4. Degrade gracefully when recommended features are absent.

## 7. Language and Runtime Neutrality

Allowed implementation examples:

1. Rust in-process adapter.
2. Go sidecar writing stream files.
3. Node.js daemon bridging app APIs.
4. Python service with filesystem bridge.

All are equally compatible if conformance tests and Core rules pass.

## 8. Claim Procedure (Practical)

Recommended sequence for adapter authors:

1. Add `contract_version` and `conformance` block to `manifest.res.json`.
2. Run static gates:
   - `APPFS_CONTRACT_TESTS=1 APPFS_STATIC_FIXTURE=1 ./tests/test-appfs-contract.sh`
3. Run live gates against mounted AppFS + running adapter:
   - `APPFS_CONTRACT_TESTS=1 APPFS_ROOT=<mounted_app_root> ./tests/test-appfs-contract.sh`
   - `APPFS_CONTRACT_TESTS=1 ./tests/appfs/run-live-with-adapter.sh`
4. Fill adapter acceptance checklist (all items with pass/fail + evidence).
5. Publish compatibility claim:
   - claim `core` only when no Core MUST violation remains
   - optionally list `recommended` and `extensions`

Suggested claim wording:

```text
This implementation claims AppFS v0.1 Core compatibility for app <app_id>,
validated by CT-001/002/003/004/005 and adapter checklist evidence dated <date>.
```
