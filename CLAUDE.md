# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

AgentFS is a specialized filesystem for AI agents, storing all agent state (files, key-value data, tool call history) in a single SQLite database file. The project consists of:

- **CLI** (`cli/`): Rust command-line tool for managing agent filesystems
- **Rust SDK** (`sdk/rust/`): Core SDK used by the CLI
- **TypeScript SDK** (`sdk/typescript/`): JavaScript/TypeScript SDK for Node.js and browsers
- **Python SDK** (`sdk/python/`): Python SDK
- **Sandbox** (`sandbox/`): Linux-only syscall interception for process isolation (uses Facebook's reverie)

## Architecture

### Three-Layer Storage Model
All agent data is stored in SQLite via the [Turso](https://github.com/tursodatabase/turso) database:

1. **Virtual Filesystem** (`fs_inode`, `fs_dentry`, `fs_data`, `fs_symlink` tables): POSIX-like filesystem with inode design, supporting hard links, symlinks, and chunked file storage
2. **Key-Value Store** (`kv_store` table): Simple get/set for agent state
3. **Tool Call Audit** (`tool_calls` table): Insert-only audit log for tool invocations

### OverlayFS
When `base` option is set, the filesystem operates as copy-on-write overlay on a host directory. Modifications are stored in the delta layer (SQLite) while the base directory remains read-only. The `fs_whiteout` table tracks deleted paths.

### Platform Differences
- **Linux**: FUSE mounting, sandbox with syscall interception (reverie)
- **macOS**: NFSv3 mounting (no FUSE), no sandbox support
- **Windows**: CLI available but limited (no mount/run commands)

## Build Commands

### Rust (CLI & SDK)
```bash
cd cli
cargo build                    # Debug build
cargo build --release          # Release build
cargo test                     # Run Rust SDK tests
cargo test --package agentfs   # Run CLI tests
```

On Linux ARM, `libunwind-dev` is required for sandbox functionality.

### TypeScript SDK
```bash
cd sdk/typescript
npm install
npm run build           # Compile TypeScript
npm test                # Run tests with vitest
npm run test:browser    # Run browser tests (Chromium + Firefox)
```

### Python SDK
```bash
cd sdk/python
uv sync                    # Install dependencies with uv
uv run pytest              # Run tests
uv run ruff check .        # Lint with ruff
uv run ruff format .       # Format code
```

## CLI Commands

The CLI binary is `agentfs`. Key commands:

```bash
agentfs init <id>                    # Create agent database at .agentfs/<id>.db
agentfs init <id> --base <dir>       # Create overlay filesystem on base directory
agentfs fs ls <id-or-path> [/path]   # List directory contents
agentfs fs cat <id-or-path> <file>   # Read file contents
agentfs fs write <id-or-path> <file> <content>  # Write to file
agentfs mount <id-or-path> <mountpoint>        # Mount filesystem (Linux: FUSE, macOS: NFS)
agentfs run [--sandbox] <command>             # Run command in sandbox with /agent mounted
agentfs exec <id-or-path> <command> [args...] # Execute command in existing agent context
agentfs diff <id-or-path>                    # Show overlay filesystem changes
agentfs timeline <id-or-path>                # Show tool call history
agentfs sync pull|push|checkpoint|stats <id-or-path>  # Sync operations
```

## SDK Usage Patterns

### Rust
```rust
let agent = AgentFS::open(AgentFSOptions::with_id("my-agent")).await?;
agent.fs.mkdir("/output", 0, 0).await?;
agent.kv.set("key", &"value").await?;
agent.tools.record("tool_name", start, end, params, result).await?;
```

### TypeScript
```typescript
import { AgentFS } from 'agentfs-sdk';

const agent = await AgentFS.open({ id: 'my-agent' });
await agent.fs.mkdir('/output');
await agent.kv.set('key', { data: 'value' });
await agent.tools.record('tool', startTs, endTs, params, result);
```

### Python
```python
from agentfs_sdk import AgentFS

agent = await AgentFS.open(id="my-agent")
await agent.fs.mkdir("/output")
await agent.kv.set("key", {"data": "value"})
await agent.tools.record("tool", start_ts, end_ts, params, result)
```

## Schema Migrations

Schema version is tracked in `schema_version` table. When schema changes, run:
```bash
agentfs migrate <id-or-path>        # Apply schema migrations
agentfs migrate <id-or-path> --dry-run  # Preview changes
```

## Testing

### CLI Integration Tests
```bash
cd cli
./tests/all.sh                   # Run all integration tests
```

### Filesystem Testing (Linux)
For POSIX compliance testing, see `TESTING.md` for pjdfstest and xfstests setup.

## Code Organization

- `sdk/rust/src/lib.rs`: Main `AgentFS` struct, options, connection pool
- `sdk/rust/src/filesystem/`: Filesystem implementations (AgentFS, OverlayFS, HostFS)
- `sdk/rust/src/kvstore.rs`: Key-value store
- `sdk/rust/src/toolcalls.rs`: Tool call tracking
- `sdk/rust/src/schema.rs`: Schema version management and migrations
- `cli/src/cmd/`: CLI command handlers
- `sandbox/src/syscall/`: Syscall interception (Linux only)
