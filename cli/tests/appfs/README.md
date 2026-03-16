# AppFS Contract Test Skeleton

This directory contains shell-first contract tests for AppFS v0.1.

## Run

```bash
cd cli
APPFS_CONTRACT_TESTS=1 ./tests/test-appfs-contract.sh
```

For static fixture validation (without mounted runtime):

```bash
cd cli
APPFS_CONTRACT_TESTS=1 APPFS_STATIC_FIXTURE=1 APPFS_ROOT=/mnt/c/Users/esp3j/rep/agentfs/examples/appfs ./tests/test-appfs-contract.sh
```

To run through the existing aggregate test entry:

```bash
cd cli
APPFS_CONTRACT_TESTS=1 ./tests/all.sh
```

For a full Linux live run (mount + fixture + `serve appfs` + contract tests):

```bash
cd cli
./tests/appfs/run-live-with-adapter.sh
```

## Environment

| Variable | Default |
|---|---|
| `APPFS_ROOT` | `/app` |
| `APPFS_APP_ID` | `aiim` |
| `APPFS_TEST_ACTION` | `/app/aiim/contacts/zhangsan/send_message.act` |
| `APPFS_STREAMING_ACTION` | `/app/aiim/files/file-001/download.act` |
| `APPFS_PAGEABLE_RESOURCE` | `/app/aiim/chats/chat-001/messages.res.json` |
| `APPFS_EXPIRED_PAGEABLE_RESOURCE` | `/app/aiim/chats/chat-expired/messages.res.json` |
| `APPFS_TIMEOUT_SEC` | `10` |
| `APPFS_STATIC_FIXTURE` | `0` |

## Live Harness Environment (`run-live-with-adapter.sh`)

| Variable | Default |
|---|---|
| `APPFS_FIXTURE_DIR` | `../examples/appfs` (from repo root) |
| `APPFS_LIVE_AGENT_ID` | `appfs-live-$$` |
| `APPFS_LIVE_MOUNTPOINT` | `/tmp/agentfs-appfs-live-$$` |
| `APPFS_APP_ID` | `aiim` |
| `APPFS_ADAPTER_POLL_MS` | `100` |
| `APPFS_TIMEOUT_SEC` | `20` |
| `APPFS_MOUNT_WAIT_SEC` | `20` |
| `APPFS_MOUNT_LOG` | `cli/appfs-mount-live.log` |
| `APPFS_ADAPTER_LOG` | `cli/appfs-adapter-live.log` |

## Notes

1. Tests are currently gated by `APPFS_CONTRACT_TESTS=1` so they do not affect existing CI by default.
2. Some checks require `jq`; if missing, JSON field-level assertions are skipped.
3. `APPFS_STATIC_FIXTURE=1` runs only static contract checks (layout/replay/manifest policy).
4. `run-live-with-adapter.sh` is Linux/FUSE oriented and expects `fusermount` + `mountpoint`.
5. Live suite also validates paging error mapping (`CT-009`), streaming lifecycle (`CT-006`), malformed submit rejection (`CT-007`), ordered multi-submit behavior (`CT-008`), in-progress write atomicity (`CT-010`), interrupted-write no-commit behavior (`CT-011`), and unsafe-path no-side-effect guard (`CT-012`).
6. This is a skeleton focused on protocol gates, not full adapter business behavior.
