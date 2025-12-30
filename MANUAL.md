# AgentFS User Manual

AgentFS is a filesystem explicitly designed for AI agents. Just as traditional filesystems provide file and directory abstractions for applications, AgentFS provides the storage abstractions that AI agents need.

## Overview

AgentFS provides the following components:

1. SDK - TypeScript and Rust libraries for programmatic filesystem access
2. CLI - Command-line interface for managing agent filesystems
3. Specification - SQLite-based agent filesystem specification
4. FUSE/NFS Mount - Mount agent filesystems on the host using FUSE (Linux) or NFS (macOS)
5. Overlay Filesystem - Copy-on-write filesystem layer over host directories
6. Sandbox - Linux-compatible execution environment with agent filesystem support (experimental)

## Quick Start

### 1. Initialize an Agent Filesystem

Create a new SQLite-based agent filesystem with an identifier:

```bash
$ agentfs init my-agent
Created agent filesystem: .agentfs/my-agent.db
Agent ID: my-agent
```

Or let AgentFS generate a unique identifier:

```bash
$ agentfs init
Created agent filesystem: .agentfs/agent-1234567890.db
Agent ID: agent-1234567890
```

Use `--force` to overwrite an existing agent filesystem:

```bash
$ agentfs init my-agent --force
Created agent filesystem: .agentfs/my-agent.db
Agent ID: my-agent
```

### 2. Mount the AgentFS filesystem with FUSE (Linux) or NFS (macOS)

Mount an AgentFS filesystem on the host:

```bash
$ agentfs mount my-agent ./my-agent-mount
```

You can then use the mounted agentfs filesystem:

```bash
$ echo "hello, agentfs!" > ./my-agent-mount/hello.txt
$ cat ./my-agent-mount/hello.txt
hello, agentfs!
```

### 3. Run Programs in the Sandbox (experimental)

Start any program with the agent filesystem mounted at `/agent`:

```bash
$ agentfs run /bin/bash
Welcome to AgentFS!

The following mount points are sandboxed:
 - /agent -> agent.db (sqlite)

$ echo "hello, agent" > /agent/hello.txt
$ cat /agent/hello.txt
hello, agent
$ exit
```

### 3. Inspect the Agent Filesystem

List files in the agent filesystem:

```bash
$ agentfs fs ls my-agent
Using agent: my-agent
f hello.txt
```

Display file contents:

```bash
$ agentfs fs cat my-agent hello.txt
hello, agent
```

You can also use a database path directly:

```bash
$ agentfs fs cat .agentfs/my-agent.db hello.txt
hello, agent
```

## AgentFS Tool Reference

### `agentfs init`

Initialize a new agent filesystem.

**Usage:**
```bash
agentfs init [OPTIONS] [ID]
```

**Arguments:**
- `[ID]` - Optional agent identifier (if not provided, generates a unique one like `agent-{timestamp}`)
  - Must contain only alphanumeric characters, hyphens, and underscores
  - Creates database at `.agentfs/{ID}.db`

**Options:**
- `--force` - Overwrite existing agent filesystem if it exists
- `--base <PATH>` - Base directory for overlay filesystem (copy-on-write)
- `-h, --help` - Print help

**Examples:**
```bash
# Create with auto-generated ID
agentfs init

# Create with custom ID
agentfs init production-agent

# Overwrite existing agent filesystem
agentfs init my-agent --force

# Create overlay filesystem with host directory as base
agentfs init my-agent --base /path/to/project
```

**What it does:**
Creates a new SQLite database in the `.agentfs/` directory with the [Agent Filesystem schema](SPEC.md), including:
- Root directory (inode 1)
- File metadata tables (`fs_inode`, `fs_dentry`, `fs_data`, `fs_symlink`)
- Key-value store table (`kv_store`)
- Tool call tracking table (`tool_calls`)

When `--base` is specified, additional overlay tables are created:
- `fs_whiteout` - Tracks deleted files from the base layer
- `fs_overlay_config` - Stores overlay configuration (base type and path)

The `.agentfs/` directory is automatically created if it doesn't exist.

### `agentfs mount`

Mount an agent filesystem using FUSE (Linux) or NFS (macOS).

**Usage:**
```bash
agentfs mount <ID_OR_PATH> <MOUNT_POINT>
```

**Arguments:**
- `<ID_OR_PATH>` - Agent ID or database path
- `<MOUNT_POINT>` - Directory where the filesystem will be mounted

**Options:**
- `-h, --help` - Print help

**Examples:**
```bash
# Mount using agent ID
agentfs mount my-agent ./my-agent-mount

# Mount using database path
agentfs mount .agentfs/my-agent.db ./my-agent-mount
```

**What it does:**
Mounts the agent filesystem as a FUSE filesystem on the host, allowing you to interact with the agent's files using standard filesystem tools (ls, cat, cp, etc.).

**Requirements:**
- Linux: FUSE must be installed on your system
- macOS: Uses NFS (no additional installation required)

**Usage after mounting:**
```bash
# Write files
echo "hello, agentfs!" > ./my-agent-mount/hello.txt

# Read files
cat ./my-agent-mount/hello.txt

# List files
ls ./my-agent-mount/
```

To unmount, use `fusermount -u ./my-agent-mount` on Linux or `umount ./my-agent-mount` on macOS.

### `agentfs run`

Execute a program in a sandboxed environment with copy-on-write filesystem isolation.

**Usage:**
```bash
agentfs run [OPTIONS] <COMMAND> [ARGS]...
```

**Arguments:**
- `<COMMAND>` - Command to execute
- `[ARGS]...` - Arguments for the command

**Options:**
- `--session <ID>` - Session identifier for sharing the delta layer across multiple runs. If not provided, a unique session ID is generated for each run. Use the same session ID to share modifications between runs or to join an existing session from another terminal.
- `--allow <PATH>` - Additional paths to allow read-write access (can be specified multiple times)
- `--no-default-allows` - Disable default allowed directories (~/.config, ~/.cache, ~/.local, ~/.claude, etc.)
- `--experimental-sandbox` - Use experimental ptrace-based syscall interception sandbox (Linux only)
- `--strace` - Enable strace-like output for system calls (only with `--experimental-sandbox`)
- `-h, --help` - Print help

**Environment Variables:**
- `AGENTFS` - Set to `1` inside the sandbox to indicate an AgentFS environment
- `AGENTFS_SANDBOX` - Set to the sandbox type (`macos-sandbox` or `linux-namespace`)

#### Platform-Specific Sandboxing

**Linux:**
Uses FUSE + overlay filesystem with user namespaces for isolation. The current working directory becomes copy-on-write (changes are stored in an AgentFS database), while the rest of the filesystem is read-only.

**macOS:**
Uses NFS + overlay filesystem with Apple's Sandbox (`sandbox-exec`) for kernel-enforced isolation. The current working directory becomes copy-on-write, and file writes are restricted to:
- Current working directory (via the copy-on-write overlay)
- `/tmp`
- Default allowed directories in HOME: `~/.claude`, `~/.config`, `~/.cache`, `~/.local`, `~/.npm`
- Any paths specified with `--allow`

All other locations are read-only. Use `--no-default-allows` to disable the default allowed directories.

**Examples:**

Basic shell access:
```bash
agentfs run /bin/bash
```

Run a Python script:
```bash
agentfs run python3 agent.py
```

Run with a named session to persist changes across runs:
```bash
# First run - creates session "my-session"
agentfs run --session my-session /bin/bash
# ... make some changes, then exit

# Second run - reuses the same delta layer
agentfs run --session my-session /bin/bash
# ... changes from the first run are still present
```

Join an existing session from another terminal:
```bash
# Terminal 1: Start a session
agentfs run --session shared-session /bin/bash

# Terminal 2: Join the same session
agentfs run --session shared-session /bin/bash
# Both terminals share the same copy-on-write filesystem
```

Access the session ID from within the sandbox:
```bash
agentfs run /bin/bash -c 'echo "Session: $AGENTFS_SESSION"'
```

Use experimental ptrace sandbox with strace output:
```bash
agentfs run --experimental-sandbox --strace python3 agent.py
```

### `agentfs fs`

Perform filesystem operations on the agent database from outside the sandbox.

**Usage:**
```bash
agentfs fs <COMMAND>
```

**Commands:**
- `ls` - List files in the filesystem
- `cat` - Display file contents

#### `agentfs fs ls`

List files and directories in the agent filesystem.

**Usage:**
```bash
agentfs fs ls <ID_OR_PATH> [FS_PATH]
```

**Arguments:**
- `<ID_OR_PATH>` - Agent ID or database path
- `[FS_PATH]` - Path within the filesystem to list (default: `/`)

**Examples:**
```bash
# List root directory using agent ID
agentfs fs ls my-agent

# List subdirectory using agent ID
agentfs fs ls my-agent /artifacts

# List using database path directly
agentfs fs ls .agentfs/my-agent.db /artifacts
```

**Output format:**
- `f <name>` - Regular file
- `d <name>` - Directory

#### `agentfs fs cat`

Display the contents of a file in the agent filesystem.

**Usage:**
```bash
agentfs fs cat <ID_OR_PATH> <FILE_PATH>
```

**Arguments:**
- `<ID_OR_PATH>` - Agent ID or database path
- `<FILE_PATH>` - Path to the file within the filesystem

**Examples:**
```bash
# Display file contents using agent ID
agentfs fs cat my-agent hello.txt

# Display file in subdirectory
agentfs fs cat my-agent /artifacts/report.txt

# Use database path directly
agentfs fs cat .agentfs/my-agent.db /artifacts/report.txt
```

### `agentfs completions`

Manage shell completions for the AgentFS CLI.

**Usage:**
```bash
agentfs completions <COMMAND>
```

**Commands:**
- `install` - Install shell completions to your shell config file
- `uninstall` - Remove shell completions from your shell config file
- `show` - Display completion configuration for all supported shells

#### `agentfs completions install`

Automatically install shell completions by appending the appropriate configuration to your shell's rc file.

**Usage:**
```bash
agentfs completions install [SHELL]
```

**Arguments:**
- `[SHELL]` - Shell to install completions for (defaults to current shell detected from `$SHELL`)
  - Supported values: `bash`, `zsh`, `fish`, `elvish`, `powershell`

**Examples:**
```bash
# Install for current shell (auto-detected)
agentfs completions install

# Install for a specific shell
agentfs completions install zsh
agentfs completions install bash
```

**What it does:**
Appends the completion source line to your shell's configuration file. Paths are determined using the [`dirs`](https://docs.rs/dirs) crate and may vary by platform:
- Bash: `$HOME/.bashrc`
- Zsh: `$HOME/.zshrc`
- Fish: `<config_dir>/fish/config.fish`
- Elvish: `<config_dir>/elvish/rc.elv`
- PowerShell: `<config_dir>/powershell/Microsoft.PowerShell_profile.ps1`

Where `<config_dir>` is the platform-specific configuration directory (e.g., `~/.config` on Linux, `~/Library/Application Support` on macOS).

After installation, restart your shell or source your config file:
```bash
source ~/.zshrc  # for zsh
source ~/.bashrc # for bash
```

#### `agentfs completions uninstall`

Remove shell completions from your shell's configuration file.

**Usage:**
```bash
agentfs completions uninstall [SHELL]
```

**Arguments:**
- `[SHELL]` - Shell to uninstall completions for (defaults to current shell)

**Examples:**
```bash
# Uninstall from current shell
agentfs completions uninstall

# Uninstall from a specific shell
agentfs completions uninstall zsh
```

#### `agentfs completions show`

Display the completion configuration lines for all supported shells. Useful if you prefer to manually add completions to your shell configuration.

**Usage:**
```bash
agentfs completions show
```

**Output:**
```
Bash (~/.bashrc):
  source <(COMPLETE=bash agentfs)

Zsh (~/.zshrc):
  source <(COMPLETE=zsh agentfs)

Fish (~/.config/fish/config.fish):
  COMPLETE=fish agentfs | source

Elvish (~/.config/elvish/rc.elv):
  eval (COMPLETE=elvish agentfs | slurp)

PowerShell (~/.config/powershell/Microsoft.PowerShell_profile.ps1):
  $env:COMPLETE = "powershell"; agentfs | Out-String | Invoke-Expression; Remove-Item Env:\COMPLETE
```

## AgentFS SDK

The AgentFS SDK provides a TypeScript/JavaScript interface for building agents that use the agent filesystem. It offers three main APIs for working with the agent database:

- **Key-Value Store** - Simple storage for agent context, preferences, and state
- **Filesystem** - POSIX-like file operations for reading/writing files
- **Tool Calls** - Track and analyze agent tool invocations

### Installation

```bash
npm install agentfs-sdk
```

### Quick Start

```typescript
import { AgentFS } from 'agentfs-sdk';

// Using id (creates .agentfs/my-agent.db)
const agent = await AgentFS.open({ id: 'my-agent' });

// Using id with custom path
const agent2 = await AgentFS.open({ id: 'my-agent', path: './data/mydb.db' });

// Using path only
const agent3 = await AgentFS.open({ path: './data/mydb.db' });

// Key-value operations
await agent.kv.set('user:name', 'Alice');
const name = await agent.kv.get('user:name');

// Filesystem operations
await agent.fs.writeFile('/output/report.txt', 'Hello, world!');
const content = await agent.fs.readFile('/output/report.txt');
const files = await agent.fs.readdir('/output');

// Tool call tracking
await agent.tools.record(
  'web_search',
  Date.now() / 1000,
  Date.now() / 1000 + 1.5,
  { query: 'AI agents' },
  { results: ['result1', 'result2'] }
);

// Get performance statistics
const stats = await agent.tools.getStats();

// Close when done
await agent.close();
```

### API Reference

#### AgentFS Class

The main class for interacting with the agent database.

**Static Method:**
```typescript
AgentFS.open(options?: AgentFSOptions): Promise<AgentFS>
```

Opens or creates an agent filesystem.

**Parameters:**
- `options: AgentFSOptions` - Configuration (at least `id` or `path` required)
  - `id?: string` - Agent identifier (if no `path`, creates `.agentfs/{id}.db`)
  - `path?: string` - Explicit database path (takes precedence over id-based path)

**Examples:**
```typescript
// Using id (creates .agentfs/my-agent.db)
const agent = await AgentFS.open({ id: 'my-agent' });

// Using id with custom path
const agent2 = await AgentFS.open({ id: 'my-agent', path: './data/mydb.db' });

// Using path only
const agent3 = await AgentFS.open({ path: './data/mydb.db' });
```

**Properties:**
- `kv: KvStore` - Key-value store interface
- `fs: Filesystem` - Filesystem interface
- `tools: ToolCalls` - Tool call tracking interface

**Methods:**
- `getDatabase(): Database` - Get the underlying Database instance
- `close(): Promise<void>` - Close the database connection

**AgentFSOptions Interface:**
```typescript
interface AgentFSOptions {
  id?: string;    // Agent identifier (if no path, creates .agentfs/{id}.db)
  path?: string;  // Explicit database path (takes precedence over id-based path)
}
```

Note: At least one of `id` or `path` must be provided.

#### Key-Value Store API

Simple key-value storage for agent context and preferences.

**set(key: string, value: any): Promise<void>**

Store a value with the given key. The value is automatically serialized to JSON.

```typescript
await agent.kv.set('config', { theme: 'dark', lang: 'en' });
await agent.kv.set('counter', 42);
await agent.kv.set('items', ['apple', 'banana', 'cherry']);
```

**get(key: string): Promise<any>**

Retrieve a value by key. Returns `undefined` if the key doesn't exist. The value is automatically deserialized from JSON.

```typescript
const config = await agent.kv.get('config');
const counter = await agent.kv.get('counter');
const missing = await agent.kv.get('nonexistent'); // undefined
```

**delete(key: string): Promise<void>**

Delete a key-value pair.

```typescript
await agent.kv.delete('counter');
```

**ready(): Promise<void>**

Wait for initialization to complete.

#### Filesystem API

POSIX-like filesystem operations for managing files and directories.

**writeFile(path: string, content: string | Buffer): Promise<void>**

Write content to a file. Creates parent directories automatically. Overwrites existing files.

```typescript
// Write text
await agent.fs.writeFile('/notes/todo.txt', 'Buy groceries');

// Write binary data
const pdfBuffer = Buffer.from(pdfData);
await agent.fs.writeFile('/reports/summary.pdf', pdfBuffer);
```

**readFile(path: string): Promise<string>**

Read file contents as a UTF-8 string. Throws `ENOENT` error if the file doesn't exist.

```typescript
const content = await agent.fs.readFile('/notes/todo.txt');
console.log(content); // 'Buy groceries'
```

**readdir(path: string): Promise<string[]>**

List files and directories in a directory. Returns file/directory names (not full paths).

```typescript
const files = await agent.fs.readdir('/notes');
console.log(files); // ['todo.txt', 'ideas.txt']
```

**deleteFile(path: string): Promise<void>**

Delete a file. Throws `ENOENT` error if the file doesn't exist.

```typescript
await agent.fs.deleteFile('/notes/todo.txt');
```

**stat(path: string): Promise<Stats>**

Get file/directory metadata.

```typescript
const stats = await agent.fs.stat('/notes/todo.txt');
console.log(stats.size);      // File size in bytes
console.log(stats.mtime);     // Modification time (Unix timestamp)
console.log(stats.isFile());  // true
console.log(stats.isDirectory()); // false
```

**Stats Interface:**
```typescript
interface Stats {
  ino: number;           // Inode number
  mode: number;          // File mode (type + permissions)
  nlink: number;         // Number of hard links
  uid: number;           // User ID
  gid: number;           // Group ID
  size: number;          // File size in bytes
  atime: number;         // Access time (Unix timestamp)
  mtime: number;         // Modification time (Unix timestamp)
  ctime: number;         // Change time (Unix timestamp)
  isFile(): boolean;
  isDirectory(): boolean;
  isSymbolicLink(): boolean;
}
```

**ready(): Promise<void>**

Wait for initialization to complete.

#### Tool Calls API

Track and analyze agent tool invocations for debugging and performance monitoring.

**record(name: string, started_at: number, completed_at: number, parameters?: any, result?: any, error?: string): Promise<number>**

Record a completed tool call. Either `result` or `error` should be provided (not both). Returns the ID of the created record.

Timestamps should be Unix timestamps (seconds since epoch).

```typescript
const started = Date.now() / 1000;
// ... perform the tool call ...
const completed = Date.now() / 1000;

// Successful call
const id = await agent.tools.record(
  'web_search',
  started,
  completed,
  { query: 'AgentFS' },
  { results: ['result1', 'result2'] }
);

// Failed call
await agent.tools.record(
  'database_query',
  started,
  completed,
  { sql: 'SELECT * FROM users' },
  undefined,
  'Connection timeout'
);
```

**get(id: number): Promise<ToolCall | undefined>**

Get a specific tool call by ID.

```typescript
const call = await agent.tools.get(42);
console.log(call.name);         // 'web_search'
console.log(call.duration_ms);  // 1500
```

**getByName(name: string, limit?: number): Promise<ToolCall[]>**

Query tool calls by name, most recent first.

```typescript
// Get all web_search calls
const searches = await agent.tools.getByName('web_search');

// Get last 10 web_search calls
const recent = await agent.tools.getByName('web_search', 10);
```

**getRecent(since: number, limit?: number): Promise<ToolCall[]>**

Query recent tool calls since a given timestamp, most recent first.

```typescript
const oneHourAgo = Date.now() / 1000 - 3600;
const recentCalls = await agent.tools.getRecent(oneHourAgo);

// Last 5 calls in the past hour
const latest = await agent.tools.getRecent(oneHourAgo, 5);
```

**getStats(): Promise<ToolCallStats[]>**

Get performance statistics for all tools, ordered by total call count.

```typescript
const stats = await agent.tools.getStats();
for (const stat of stats) {
  console.log(`${stat.name}:`);
  console.log(`  Total calls: ${stat.total_calls}`);
  console.log(`  Success rate: ${stat.successful / stat.total_calls * 100}%`);
  console.log(`  Avg duration: ${stat.avg_duration_ms}ms`);
}
```

**ToolCall Interface:**
```typescript
interface ToolCall {
  id: number;
  name: string;
  parameters?: any;
  result?: any;
  error?: string;
  started_at: number;      // Unix timestamp (seconds)
  completed_at: number;    // Unix timestamp (seconds)
  duration_ms: number;
}
```

**ToolCallStats Interface:**
```typescript
interface ToolCallStats {
  name: string;
  total_calls: number;
  successful: number;
  failed: number;
  avg_duration_ms: number;
}
```

**ready(): Promise<void>**

Wait for initialization to complete.

### Examples

The SDK includes working examples in the `sdk/examples/` directory:

- **Key-Value Store** (`sdk/examples/kvstore/`) - Basic key-value operations
- **Filesystem** (`sdk/examples/filesystem/`) - File and directory operations
- **Tool Calls** (`sdk/examples/toolcalls/`) - Tool call tracking and analytics

Run examples:
```bash
cd sdk/examples/kvstore
npm install
npm start
```

### TypeScript Support

The SDK is written in TypeScript and includes full type definitions. TypeScript users get autocomplete, type checking, and inline documentation:

```typescript
import { AgentFS, AgentFSOptions, Stats, ToolCall, ToolCallStats } from 'agentfs-sdk';

const agent = await AgentFS.open({ id: 'my-agent' });

// Type-safe operations
const stats: Stats = await agent.fs.stat('/file.txt');
const calls: ToolCall[] = await agent.tools.getByName('search');
```

### Error Handling

The SDK throws standard Node.js-style errors with descriptive messages:

```typescript
try {
  await agent.fs.readFile('/nonexistent.txt');
} catch (error) {
  console.error(error.message); // "ENOENT: no such file or directory, open '/nonexistent.txt'"
}

try {
  await agent.fs.deleteFile('/missing.txt');
} catch (error) {
  console.error(error.message); // "ENOENT: no such file or directory, unlink '/missing.txt'"
}
```

### Using with Turso

The SDK uses [@tursodatabase/database](https://www.npmjs.com/package/@tursodatabase/database) under the hood, which supports both local SQLite files and remote Turso databases.

For local SQLite (default behavior):
```typescript
const agent = await AgentFS.open({ id: 'my-agent' });
```

For remote Turso databases, you'll need to use the underlying database directly:
```typescript
import { Database } from '@tursodatabase/database';

const db = new Database('libsql://your-database.turso.io', {
  authToken: process.env.TURSO_AUTH_TOKEN
});

// Use db with AgentFS components directly
```

See the [Turso documentation](https://docs.turso.tech) for more details on remote databases.

## Advanced Usage

### Overlay Filesystem (Copy-on-Write)

The overlay filesystem feature allows agents to work on a copy-on-write view of an existing directory. This is useful for:

- **Safe experimentation**: Let agents modify files without affecting the original project
- **Reproducible workspaces**: Reset agent changes by simply deleting the delta database
- **Efficient storage**: Only modified files are stored in the database, not the entire directory

#### How It Works

An overlay filesystem combines two layers:
1. **Base layer (read-only)**: The original host directory
2. **Delta layer (writable)**: An AgentFS database storing modifications

When reading files:
- Files in the delta layer take precedence
- If not in delta, reads fall through to the base layer
- Deleted files are tracked via "whiteouts" to hide base layer entries

When writing files:
- New files go directly to the delta layer
- Modifying base files triggers copy-on-write (file is copied to delta first)
- All changes are isolated in the database

#### Example: Agent Working on a Project

```bash
# Initialize an overlay filesystem with your project as the base
agentfs init agent-workspace --base /home/user/myproject

# Mount the overlay filesystem
mkdir /tmp/workspace
agentfs mount agent-workspace /tmp/workspace

# The agent can now freely modify files in /tmp/workspace
# All changes are stored in .agentfs/agent-workspace.db
# The original /home/user/myproject remains untouched
```

#### Resetting Changes

To discard all agent changes and start fresh:

```bash
# Remove the delta database
rm .agentfs/agent-workspace.db

# Re-initialize
agentfs init agent-workspace --base /home/user/myproject
```

#### Inspecting Changes

Since all modifications are stored in SQLite, you can inspect what the agent changed:

```bash
# See modified/created files
sqlite3 .agentfs/agent-workspace.db "SELECT name FROM fs_dentry"

# See deleted files (whiteouts)
sqlite3 .agentfs/agent-workspace.db "SELECT path FROM fs_whiteout"
```

### Copy-on-Write Isolation

When you run `agentfs run`, the current working directory becomes a copy-on-write overlay:

- **Base layer**: Your actual filesystem (read-only access via HostFS)
- **Delta layer**: An AgentFS database that captures all changes

Any modifications (new files, edits, deletions) are stored in the delta database, leaving your actual filesystem untouched. When you exit, changes are discarded unless you explicitly export them using `agentfs diff`.

### Debugging with Strace

Use `--strace` with the experimental ptrace sandbox to see all intercepted system calls:

```bash
agentfs run --experimental-sandbox --strace python3 script.py
```

This shows detailed information about every filesystem operation, useful for debugging and understanding agent behavior.

### Snapshotting Agent State

Since the entire filesystem is a single SQLite file, snapshotting is trivial:

```bash
# Create a snapshot of an agent
cp .agentfs/my-agent.db .agentfs/my-agent-snapshot-$(date +%s).db

# Restore from snapshot
cp .agentfs/my-agent-snapshot-1234567890.db .agentfs/my-agent.db
```

### Querying Agent Data

You can query the agent database directly with SQLite:

```bash
sqlite3 .agentfs/my-agent.db "SELECT * FROM fs_inode WHERE mode & 0170000 = 0100000"
```

Or use the SQL interface from your application to analyze agent behavior, search files, track tool usage, etc.

See the [Agent Filesystem Specification](SPEC.md) for the complete schema.

## Learn More

- **[Agent Filesystem Specification](SPEC.md)** - Complete technical specification of the filesystem schema
- **[SDK Examples](sdk/examples/)** - Working code examples
- **[README](README.md)** - Project overview and motivation

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

## License

MIT
