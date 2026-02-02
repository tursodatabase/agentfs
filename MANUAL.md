# AgentFS Reference Guide

Command-line reference for the AgentFS CLI.

For guides, tutorials, and SDK documentation, see [docs.turso.tech/agentfs](https://docs.turso.tech/agentfs).

## Installation

```bash
curl -fsSL https://github.com/tursodatabase/agentfs/releases/latest/download/agentfs-installer.sh | sh
```

## Commands

### agentfs init

Initialize a new agent filesystem.

```
agentfs init [OPTIONS] [ID]
```

**Arguments:**
- `ID` - Agent identifier (default: `agent-{timestamp}`)

**Options:**
- `--force` - Overwrite existing agent filesystem
- `--base <PATH>` - Base directory for overlay filesystem (copy-on-write)
- `--key <KEY>` - Hex-encoded encryption key for local encryption
- `--cipher <CIPHER>` - Cipher algorithm (required with `--key`)
- `--sync-remote-url <URL>` - Remote Turso database URL for sync
- `--sync-partial-prefetch` - Enable prefetching for partial sync
- `--sync-partial-segment-size <SIZE>` - Segment size for partial sync
- `--sync-partial-bootstrap-query <QUERY>` - Custom bootstrap query
- `--sync-partial-bootstrap-length <LENGTH>` - Bootstrap prefix length

**Note:** Local encryption and cloud sync cannot be used together.

**Options (continued):**
- `-c, --command <CMD>` - Command to execute after initialization (see below)
- `--backend <BACKEND>` - Mount backend for `-c` option (`fuse` or `nfs`)

**Running a command after init:**

The `-c` option initializes the filesystem, mounts it to a temporary directory, runs the specified command with that directory as the working directory, then automatically unmounts.

```bash
# Initialize and run a command in the new filesystem
agentfs init my-agent -c "touch hello.txt && ls -la"

# With overlay filesystem
agentfs init my-overlay --base /path/to/project -c "make build"
```

### agentfs exec

Execute a command with an AgentFS filesystem mounted (Unix only).

```
agentfs exec [OPTIONS] <ID_OR_PATH> <COMMAND> [ARGS]...
```

Mounts the specified AgentFS to a temporary directory, runs the command with that directory as the working directory, then automatically unmounts. This is useful for running tools that need filesystem access without a persistent mount.

If the AgentFS was initialized with `--base` (overlay mode), the overlay filesystem is used automatically.

**Arguments:**
- `ID_OR_PATH` - Agent identifier or database path
- `COMMAND` - Command to execute
- `ARGS` - Arguments for the command

**Options:**
- `--backend <BACKEND>` - Mount backend (`fuse` on Linux, `nfs` on macOS by default)
- `--key <KEY>` - Hex-encoded encryption key for encrypted databases
- `--cipher <CIPHER>` - Cipher algorithm (required with `--key`)

**Examples:**

```bash
# Run ls in the AgentFS root
agentfs exec my-agent ls -la

# Run a build command
agentfs exec my-overlay make build

# With encryption
agentfs exec my-agent --key $KEY --cipher aes256gcm cat /config.json
```

### agentfs run

Execute a program in a sandboxed environment with copy-on-write filesystem.

```
agentfs run [OPTIONS] <COMMAND> [ARGS]...
```

**Options:**
- `--session <ID>` - Named session for persistence across runs
- `--allow <PATH>` - Allow write access to additional directories (repeatable)
- `--no-default-allows` - Disable default allowed directories
- `--key <KEY>` - Hex-encoded encryption key for delta layer
- `--cipher <CIPHER>` - Cipher algorithm (required with `--key`)
- `--experimental-sandbox` - Use ptrace-based syscall interception (Linux only)
- `--strace` - Show intercepted syscalls (requires `--experimental-sandbox`)

**Platform behavior:**

Linux uses FUSE + overlay filesystem with user namespaces. macOS uses NFS + overlay filesystem with Apple's Sandbox.

Default allowed directories (macOS): `~/.claude`, `~/.codex`, `~/.config`, `~/.cache`, `~/.local`, `~/.npm`, `/tmp`

### agentfs mount

Mount an agent filesystem or list mounted filesystems.

```
agentfs mount [OPTIONS] [ID_OR_PATH] [MOUNT_POINT]
```

Without arguments, lists all mounted agentfs filesystems.

**Options:**
- `-a, --auto-unmount` - Automatically unmount on exit
- `--allow-root` - Allow root user to access filesystem
- `-f, --foreground` - Run in foreground
- `--uid <UID>` - User ID for all files
- `--gid <GID>` - Group ID for all files

**Unmounting:**
- Linux: `fusermount -u <MOUNT_POINT>`
- macOS: `umount <MOUNT_POINT>`

### agentfs serve mcp

Start an MCP (Model Context Protocol) server.

```
agentfs serve mcp <ID_OR_PATH> [OPTIONS]
```

**Options:**
- `--tools <TOOLS>` - Comma-separated list of tools to expose (default: all)

**Available tools:**

Filesystem: `read_file`, `write_file`, `readdir`, `mkdir`, `remove`, `rename`, `stat`, `access`

Key-Value: `kv_get`, `kv_set`, `kv_delete`, `kv_list`

### agentfs serve nfs

Start an NFS server to export AgentFS over the network.

```
agentfs serve nfs <ID_OR_PATH> [OPTIONS]
```

**Options:**
- `--bind <IP>` - IP address to bind (default: `127.0.0.1`)
- `--port <PORT>` - Port to listen on (default: `11111`)

**Mounting from client:**
```bash
mount -t nfs -o vers=3,tcp,port=11111,mountport=11111,nolock <HOST>:/ <MOUNT_POINT>
```

### agentfs sync

Synchronize agent filesystem with a remote Turso database.

```
agentfs sync <ID_OR_PATH> <SUBCOMMAND>
```

**Subcommands:**
- `pull` - Pull remote changes
- `push` - Push local changes
- `stats` - View sync statistics
- `checkpoint` - Create checkpoint

### agentfs migrate

Migrate database schema to the current version.

```
agentfs migrate [OPTIONS] <ID_OR_PATH>
```

Upgrades an AgentFS database schema to the latest version. This is necessary when using databases created with older versions of AgentFS.

**Arguments:**
- `ID_OR_PATH` - Agent identifier or database path

**Options:**
- `--dry-run` - Preview migration without applying changes

**Examples:**

```bash
# Preview pending migrations
agentfs migrate my-agent --dry-run

# Apply migrations
agentfs migrate my-agent

# Migrate using database path
agentfs migrate .agentfs/my-agent.db
```

**Output:**

The command displays the current and target schema versions, then applies any necessary migrations:

```
Database: .agentfs/my-agent.db
Current schema version: v0.2
Target schema version: v0.4

Applying migrations...
  Migrating v0.2 -> v0.4...
    Added atime_nsec column to fs_inode
    Added mtime_nsec column to fs_inode
    Added ctime_nsec column to fs_inode
    Added rdev column to fs_inode
  v0.2 -> v0.4 migration complete.

Migration completed successfully.
```

**Notes:**
- Migrations are idempotent and safe to run multiple times
- Always backup your database before running migrations on production data

### agentfs fs

Filesystem operations on agent databases.

**Common Options:**
- `--key <KEY>` - Hex-encoded encryption key for encrypted databases
- `--cipher <CIPHER>` - Cipher algorithm (required with `--key`)

#### agentfs fs ls

```
agentfs fs <ID_OR_PATH> [OPTIONS] ls [FS_PATH]
```

List files and directories. Output: `f <name>` for files, `d <name>` for directories.

#### agentfs fs cat

```
agentfs fs <ID_OR_PATH> [OPTIONS] cat <FILE_PATH>
```

Display file contents.

#### agentfs fs write

```
agentfs fs <ID_OR_PATH> [OPTIONS] write <FILE_PATH> <CONTENT>
```

Write content to a file.

### agentfs diff

Show filesystem changes in overlay mode.

```
agentfs diff <ID_OR_PATH>
```

### agentfs timeline

Display agent action timeline from the tool call audit log.

```
agentfs timeline [OPTIONS] <ID_OR_PATH>
```

**Options:**
- `--limit <N>` - Limit entries (default: 100)
- `--filter <TOOL>` - Filter by tool name
- `--status <STATUS>` - Filter by status: `pending`, `success`, `error`
- `--format <FORMAT>` - Output format: `table`, `json` (default: table)

### agentfs completions

Manage shell completions.

```
agentfs completions install [SHELL]
agentfs completions uninstall [SHELL]
agentfs completions show
```

Supported shells: `bash`, `zsh`, `fish`, `powershell`

## Environment Variables

**Configuration variables:**

| Variable | Description |
|----------|-------------|
| `AGENTFS_KEY` | Default encryption key (hex-encoded) |
| `AGENTFS_CIPHER` | Default cipher algorithm |
| `TURSO_DB_AUTH_TOKEN` | Authentication token for cloud sync |

**Variables set inside the sandbox:**

| Variable | Description |
|----------|-------------|
| `AGENTFS` | Set to `1` inside AgentFS sandbox |
| `AGENTFS_SANDBOX` | Sandbox type: `macos-sandbox` or `linux-namespace` |
| `AGENTFS_SESSION` | Current session ID |

## Local Encryption

AgentFS supports encrypting the local SQLite database at rest using libSQL's encryption feature.

**Supported ciphers:**
- `aes256gcm` - AES-256-GCM (requires 64-character hex key)
- `aes128gcm` - AES-128-GCM (requires 32-character hex key)
- `aegis256` - AEGIS-256 (requires 64-character hex key)
- `aegis128l` - AEGIS-128L (requires 32-character hex key)
- `aegis128x2`, `aegis128x4`, `aegis256x2`, `aegis256x4` - AEGIS variants

**Example: Create an encrypted filesystem**

```bash
# Generate a 256-bit key (64 hex characters)
KEY=$(openssl rand -hex 32)

# Initialize with encryption
agentfs init --key $KEY --cipher aes256gcm my-secure-agent

# Access the filesystem
agentfs fs my-secure-agent --key $KEY --cipher aes256gcm ls /
```

**Example: Encrypted sandbox session**

```bash
agentfs run --key $KEY --cipher aes256gcm -- bash
```

**Using environment variables:**

```bash
export AGENTFS_KEY=$(openssl rand -hex 32)
export AGENTFS_CIPHER=aes256gcm

agentfs init my-secure-agent
agentfs fs my-secure-agent ls /
```

**Limitations:**
- Local encryption cannot be used with cloud sync (`--sync-remote-url`)

## Files

- `.agentfs/<ID>.db` - Agent filesystem database
- `~/.config/agentfs/` - Configuration directory

## See Also

- [AgentFS Documentation](https://docs.turso.tech/agentfs) - Guides, tutorials, SDK docs
- [AgentFS Specification](SPEC.md) - SQLite schema specification
- [GitHub Repository](https://github.com/tursodatabase/agentfs) - Source code and examples
