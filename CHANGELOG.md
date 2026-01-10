# Changelog

## [0.5.3] - 2026-01-10

### Added

- `agentfs ps` command to list active sessions.
- `agentfs prune mounts` command.

### Changed

- `~/.cache`, `~/.gemini`, `~/.amp` added to default read-write allow list in `agentfs run`.
- Group paths by parent directory in `agentfs run` welcome banner.

### Performance

- Rust SDK: Switch to prepared statement caching.

### Fixed

- Rust SDK: Return ENOENT instead of EIO for file not found errors.

## [0.5.2] - 2026-01-09

### Fixed

- Fix Turso dependency.

## [0.5.1] - 2026-01-09

### Performance

- Rust SDK: Add dentry cache and path resolution optimizations.
- Rust SDK: Add in-memory whiteout cache.
- Rust SDK: Add NormalizedPath type for overlay filesystem.
- Update Turso to 0.4.3 pre-release to fix WAL read amplification.

### Fixed

- Inode consistency after copy-up in overlay filesystem.
- `unlink()` path cache invalidation in FUSE module.

### Documentation

- Update installation command for AgentFS CLI.

## [0.5.0] - 2026-01-08

### Added

- `agentfs serve` command for NFS and MCP servers.
- `agentfs mount` command to list all mounted filesystems.
- `agentfs timeline` command to display agent actions.
- `agentfs mcp-server` command.
- Basic sync support to the agentfs CLI.
- Hard link support across Rust SDK, sandbox, and FUSE.
- Local file locking on macOS.
- Explicit sandbox feature in CLI.

### Changed

- `~/.codex` added to default read-write allow list in `agentfs run`.
- Update just-bash to 2.0.
- Restructure `agentfs run` command files for clarity.

### Documentation

- Add FAQ entry for `git worktrees`.
- Add installation guide to README and MANUAL.md.

## [0.4.1] - 2026-01-02

### Added

- Cloudflare Durable Objects integration prototype.
- Sandbox: Intercept `rmdir` system call.

### Changed

- Init tracing subscriber to allow debugging turso_core.

### Fixed

- FUSE overlay deadlock.
- SIGTERM signal handling for graceful shutdown.
- `agentfs run` help text.
- ARM build for `rmdir` syscall.

## [0.4.0] - 2025-12-31

### Added

- `agentfs run` command with overlay filesystem for sandboxed execution.
- `agentfs diff` command to show filesystem changes.
- Multi-session support with `--session` flag and `AGENTFS_SESSION` environment variable.
- `--allow` flag for specifying writable directories in sandbox.
- macOS Sandbox support for filesystem isolation.
- NFS-based `agentfs run` support for macOS.
- Linux ARM64 support.
- TypeScript SDK: `FileSystem` interface for filesystem operations.
- TypeScript SDK: New APIs (`access`, `copyFile`, `rmdir`, `rename`).
- TypeScript SDK: `agentfs()` convenience function for just-bash integration.
- Python SDK: Python 3.10+ support.
- Rust SDK: `base` option for `agentfs::open`.
- Rust SDK: VFS-style `File` trait for efficient file handle operations.
- Rust SDK: `get_runtime()` helper for runtime initialization.
- FUSE: Symlink support.
- FUSE: `readdir_plus` optimization to eliminate N+1 queries.
- Database: `nlink` column for O(1) link count lookups.
- Sandbox: Intercept `chmod` system call.
- `--version` flag using git tags.
- Firecracker + AgentFS example.
- AI SDK + just-bash example with AgentFS integration.

### Changed

- Default shell is now bash on Linux, zsh on macOS.
- `/tmp` is writable by default in sandbox.
- `~/.bun` added to default allowed directories.
- npm local registry added to allowlist.
- `AGENTFS_SANDBOX` environment variable is more descriptive.
- FUSE optimizations: async read, parallel directory operations, symlink/directory caching.
- Rust SDK: Configure busy timeout instead of failing immediately.

### Fixed

- Overlay filesystem nested `pwrite()`.
- `O_APPEND` not appending to file.
- FUSE error handling.
- Execute permissions in FUSE mount.
- SSH inside user namespace by bypassing system configs.
- Symlink handling in FUSE and overlay filesystem.
- Rust SDK: `pread()` for sparse files.
- Rust SDK: `pwrite()` buffer flushing before returning.
- Rust SDK: `resolve` to prioritize agent ID over file path.
- `agentfs init --force` to reinitialize agent filesystem.
- Overlay mount I/O error by unifying whiteout schema in SDK.
- UID/GID mapping to use current user instead of root.

### Removed

- macFUSE support on macOS (replaced by NFS).

## [0.3.1] - 2025-12-17

- This is the exact same version as 0.3.0, but had to bump version number
  to work around a previous accidental publish on PyPI.

## [0.3.0] - 2025-12-17

### Added

- Python SDK for AgentFS.
- Web browser support for TypeScript SDK.
- Dynamic CLI completions with `completion` command (install/uninstall).

### Changed

- TypeScript SDK: Remove `ready()` method from the API.
- TypeScript SDK: Improve `KvStore.get()` API.
- TypeScript SDK: Improve `FileSystem.readFile()` compatibility.
- TypeScript SDK: Use `RETURNING` clause instead of `lastInsertRowid`.
- TypeScript SDK: Switch to proper Turso dependency versioning.
- Python SDK: Use `RETURNING` clause instead of `lastInsertRowid`.
- Rust SDK: Use `RETURNING` clause instead of `lastInsertRowid`.

### Fixed

- CLI `cat` command bug.
- macFUSE: pass full path to open dynamic libs.

### Documentation

- Add FAQ entry for Docker Sandbox.

## [0.2.3] - 2025-12-10

### Added

- macFUSE support

## [0.2.2] - 2025-12-08

### Added

- Linux/arm64 support.

### Documentation

- Improved FUSE module documentation.

## [0.2.1] - 2025-12-04

### Fixed

- Fix `_Unwind_RaiseException` symbol lookup error on Fedora by linking to `libgcc_s.so` dynamically.
- Eliminate dependency to libfuse by using the `fuser` crate pure Rust FUSE implementation.

## [0.2.0] - 2025-12-04

### Added

- AgentFS FUSE module for mounting agent filesystems.
- TypeScript SDK: Support for custom agent filesystem path.

### Changed

- Switch to fixed-size chunks in AgentFS specification.
- TypeScript SDK: Switch to fixed-size inode chunks.
- Rust SDK: Switch to fixed-size inode chunks.
- Switch AgentFS SDK to use identifier-based API.

## [0.1.2] - 2025-11-14

### Added

- Enable Darwin/x86-64 builds for the CLI.

## [0.1.1] - 2025-11-14

### Added

- Example using OpenAI Agents SDK and AgentFS.
- Example using Claude Agent SDK and AgentFS.

### Fixed

- CLI `ls` command now recursively lists all files.

## [0.1.0] - 2025-11-13

### Added

- Initial release of AgentFS CLI.
- TypeScript SDK with async factory method (`AgentFS.open()`).
- Sandbox command for running agents in isolated environments.
- Passthrough VFS for transparent filesystem access.
- Symlink syscall support in sandbox.
- Cross-platform builds (Linux, macOS).
- Example agent implementations.

[0.5.3]: https://github.com/tursodatabase/agentfs/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/tursodatabase/agentfs/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/tursodatabase/agentfs/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/tursodatabase/agentfs/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/tursodatabase/agentfs/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/tursodatabase/agentfs/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/tursodatabase/agentfs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/tursodatabase/agentfs/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/tursodatabase/agentfs/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/tursodatabase/agentfs/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/tursodatabase/agentfs/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/tursodatabase/agentfs/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/tursodatabase/agentfs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/tursodatabase/agentfs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/tursodatabase/agentfs/releases/tag/v0.1.0
