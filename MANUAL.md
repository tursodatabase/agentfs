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
- `--git <URL>` - Git repository URL to use as the base filesystem
- `--branch <NAME>` - Git branch to checkout (only with `--git`)
- `--depth <N>` - Shallow clone depth for faster cloning (only with `--git`)
- `--sync-remote-url <URL>` - Remote Turso database URL for sync
- `--sync-partial-prefetch` - Enable prefetching for partial sync
- `--sync-partial-segment-size <SIZE>` - Segment size for partial sync
- `--sync-partial-bootstrap-query <QUERY>` - Custom bootstrap query
- `--sync-partial-bootstrap-length <LENGTH>` - Bootstrap prefix length

### agentfs run

Execute a program in a sandboxed environment with copy-on-write filesystem.

```
agentfs run [OPTIONS] <COMMAND> [ARGS]...
```

**Options:**
- `--session <ID>` - Named session for persistence across runs
- `--allow <PATH>` - Allow write access to additional directories (repeatable)
- `--no-default-allows` - Disable default allowed directories
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

### agentfs fs

Filesystem operations on agent databases.

#### agentfs fs ls

```
agentfs fs ls <ID_OR_PATH> [FS_PATH]
```

List files and directories. Output: `f <name>` for files, `d <name>` for directories.

#### agentfs fs cat

```
agentfs fs cat <ID_OR_PATH> <FILE_PATH>
```

Display file contents.

#### agentfs fs write

```
agentfs fs write <ID_OR_PATH> <FILE_PATH> <CONTENT>
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

Variables set inside the sandbox:

| Variable | Description |
|----------|-------------|
| `AGENTFS` | Set to `1` inside AgentFS sandbox |
| `AGENTFS_SANDBOX` | Sandbox type: `macos-sandbox` or `linux-namespace` |
| `AGENTFS_SESSION` | Current session ID |

## Files

- `.agentfs/<ID>.db` - Agent filesystem database
- `~/.config/agentfs/` - Configuration directory

## See Also

- [AgentFS Documentation](https://docs.turso.tech/agentfs) - Guides, tutorials, SDK docs
- [AgentFS Specification](SPEC.md) - SQLite schema specification
- [GitHub Repository](https://github.com/tursodatabase/agentfs) - Source code and examples
