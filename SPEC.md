# Agent Filesystem Specification

**Version:** 0.4

## Introduction

The Agent Filesystem Specification defines a SQLite schema for representing agent filesystem state. The specification consists of three main components:

1. **Tool Call Audit Trail**: Captures tool invocations, parameters, and results for debugging, auditing, and performance analysis
2. **Virtual Filesystem**: Stores agent artifacts (files, documents, outputs) using a Unix-like inode design with support for hard links, proper metadata, and efficient file operations
3. **Key-Value Store**: Provides simple get/set operations for agent context, preferences, and structured state that doesn't fit into the filesystem model

All timestamps in this specification use Unix epoch format (seconds since 1970-01-01 00:00:00 UTC) with optional nanosecond precision via separate `_nsec` columns.

## Tool Calls

The tool call tracking schema captures tool invocations for debugging, auditing, and analysis.

### Schema

#### Table: `tool_calls`

Stores individual tool invocations with parameters and results. This is an insert-only audit log.

```sql
CREATE TABLE tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  parameters TEXT,
  result TEXT,
  error TEXT,
  started_at INTEGER NOT NULL,
  completed_at INTEGER NOT NULL,
  duration_ms INTEGER NOT NULL
)

CREATE INDEX idx_tool_calls_name ON tool_calls(name)
CREATE INDEX idx_tool_calls_started_at ON tool_calls(started_at)
```

**Fields:**

- `id` - Unique tool call identifier
- `name` - Tool name (e.g., 'read_file', 'web_search', 'execute_code')
- `parameters` - JSON-serialized input parameters (NULL if no parameters)
- `result` - JSON-serialized result (NULL if error)
- `error` - Error message (NULL if success)
- `started_at` - Invocation timestamp (Unix timestamp, seconds)
- `completed_at` - Completion timestamp (Unix timestamp, seconds)
- `duration_ms` - Execution duration in milliseconds

### Operations

#### Record Tool Call

```sql
INSERT INTO tool_calls (name, parameters, result, error, started_at, completed_at, duration_ms)
VALUES (?, ?, ?, ?, ?, ?, ?)
```

**Note:** Insert once when the tool call completes. Either `result` or `error` should be set, not both.

#### Query Tool Calls by Name

```sql
SELECT * FROM tool_calls
WHERE name = ?
ORDER BY started_at DESC
```

#### Query Recent Tool Calls

```sql
SELECT * FROM tool_calls
WHERE started_at > ?
ORDER BY started_at DESC
```

#### Analyze Tool Performance

```sql
SELECT
  name,
  COUNT(*) as total_calls,
  SUM(CASE WHEN error IS NULL THEN 1 ELSE 0 END) as successful,
  SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END) as failed,
  AVG(duration_ms) as avg_duration_ms
FROM tool_calls
GROUP BY name
ORDER BY total_calls DESC
```

### Consistency Rules

1. Exactly one of `result` or `error` SHOULD be non-NULL (mutual exclusion)
2. `completed_at` MUST always be set (no NULL values)
3. `duration_ms` MUST always be set and equal to `(completed_at - started_at) * 1000`
4. Parameters and results MUST be valid JSON strings when present
5. Records MUST NOT be updated or deleted (insert-only audit log)

### Implementation Notes

- This is an insert-only audit log - no updates or deletes
- Insert the record once when the tool call completes
- Set either `result` (on success) or `error` (on failure), but not both
- `parameters`, `result`, and `error` are stored as JSON-serialized strings
- `duration_ms` should be computed as `(completed_at - started_at) * 1000`
- Use indexes for efficient queries by name or time
- Consider periodic archival of old tool call records to a separate table

### Extension Points

Implementations MAY extend the tool call schema with additional functionality:

- Session/conversation grouping (add `session_id` field)
- User attribution (add `user_id` field)
- Cost tracking (add `cost` field for API calls)
- Parent/child relationships for nested tool calls
- Token usage tracking
- Input/output size metrics

Such extensions SHOULD use separate tables to maintain referential integrity.

## Virtual Filesystem

The virtual filesystem provides POSIX-like file operations for agent artifacts. The filesystem separates namespace (paths and names) from data (file content and metadata) using a Unix-like inode design. This enables hard links (multiple paths to the same file), efficient file operations, proper file metadata (permissions, timestamps), and chunked content storage.

### Schema

#### Table: `fs_config`

Stores filesystem-level configuration. This table is initialized once when the filesystem is created and MUST NOT be modified afterward.

```sql
CREATE TABLE fs_config (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)
```

**Fields:**

- `key` - Configuration key
- `value` - Configuration value (stored as text)

**Required Configuration:**

| Key | Description | Default |
|-----|-------------|---------|
| `chunk_size` | Size of data chunks in bytes | `4096` |

**Notes:**

- `chunk_size` determines the fixed size of data chunks in `fs_data`
- All chunks except the last chunk of a file are exactly `chunk_size` bytes
- Configuration is immutable after filesystem initialization
- Implementations MAY define additional configuration keys

#### Table: `fs_inode`

Stores file and directory metadata.

```sql
CREATE TABLE fs_inode (
  ino INTEGER PRIMARY KEY AUTOINCREMENT,
  mode INTEGER NOT NULL,
  nlink INTEGER NOT NULL DEFAULT 0,
  uid INTEGER NOT NULL DEFAULT 0,
  gid INTEGER NOT NULL DEFAULT 0,
  size INTEGER NOT NULL DEFAULT 0,
  atime INTEGER NOT NULL,
  mtime INTEGER NOT NULL,
  ctime INTEGER NOT NULL,
  rdev INTEGER NOT NULL DEFAULT 0,
  atime_nsec INTEGER NOT NULL DEFAULT 0,
  mtime_nsec INTEGER NOT NULL DEFAULT 0,
  ctime_nsec INTEGER NOT NULL DEFAULT 0
)
```

**Fields:**

- `ino` - Inode number (unique identifier)
- `mode` - File type and permissions (Unix mode bits)
- `nlink` - Number of hard links pointing to this inode
- `uid` - Owner user ID
- `gid` - Owner group ID
- `size` - Total file size in bytes
- `atime` - Last access time (Unix timestamp, seconds)
- `mtime` - Last modification time (Unix timestamp, seconds)
- `ctime` - Creation/change time (Unix timestamp, seconds)
- `rdev` - Device number for character and block devices (major/minor encoded)
- `atime_nsec` - Nanosecond component of last access time (0–999999999)
- `mtime_nsec` - Nanosecond component of last modification time (0–999999999)
- `ctime_nsec` - Nanosecond component of creation/change time (0–999999999)

**Mode Encoding:**

The `mode` field combines file type and permissions:

```
File type (upper bits):
  0o170000 - File type mask (S_IFMT)
  0o100000 - Regular file (S_IFREG)
  0o040000 - Directory (S_IFDIR)
  0o120000 - Symbolic link (S_IFLNK)
  0o010000 - FIFO/named pipe (S_IFIFO)
  0o020000 - Character device (S_IFCHR)
  0o060000 - Block device (S_IFBLK)
  0o140000 - Socket (S_IFSOCK)

Permissions (lower 12 bits):
  0o000777 - Permission bits (rwxrwxrwx)

Example:
  0o100644 - Regular file, rw-r--r--
  0o040755 - Directory, rwxr-xr-x
```

**Special Inodes:**

- Inode 1 MUST be the root directory

#### Table: `fs_dentry`

Maps names to inodes (directory entries).

```sql
CREATE TABLE fs_dentry (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  parent_ino INTEGER NOT NULL,
  ino INTEGER NOT NULL,
  UNIQUE(parent_ino, name)
)

CREATE INDEX idx_fs_dentry_parent ON fs_dentry(parent_ino, name)
```

**Fields:**

- `id` - Internal entry ID
- `name` - Basename (filename or directory name)
- `parent_ino` - Parent directory inode number
- `ino` - Inode this entry points to

**Constraints:**

- `UNIQUE(parent_ino, name)` - No duplicate names in a directory

**Notes:**

- Root directory (ino=1) has no dentry (no parent)
- Multiple dentries MAY point to the same inode (hard links)
- Link count is stored in `fs_inode.nlink` and must be incremented/decremented when dentries are added/removed

#### Table: `fs_data`

Stores file content in fixed-size chunks. Chunk size is configured at filesystem level via `fs_config`.

```sql
CREATE TABLE fs_data (
  ino INTEGER NOT NULL,
  chunk_index INTEGER NOT NULL,
  data BLOB NOT NULL,
  PRIMARY KEY (ino, chunk_index)
)
```

**Fields:**

- `ino` - Inode number
- `chunk_index` - Zero-based chunk index (chunk 0 contains bytes 0 to chunk_size-1)
- `data` - Binary content (BLOB), exactly `chunk_size` bytes except for the last chunk

**Notes:**

- Directories MUST NOT have data chunks
- Chunk size is determined by the `chunk_size` value in `fs_config`
- All chunks except the last chunk of a file MUST be exactly `chunk_size` bytes
- The last chunk MAY be smaller than `chunk_size`
- Byte offset for a chunk = `chunk_index * chunk_size`
- To read at byte offset `N`: `chunk_index = N / chunk_size`, `offset_in_chunk = N % chunk_size`

#### Table: `fs_symlink`

Stores symbolic link targets.

```sql
CREATE TABLE fs_symlink (
  ino INTEGER PRIMARY KEY,
  target TEXT NOT NULL
)
```

**Fields:**

- `ino` - Inode number of the symlink
- `target` - Target path (may be absolute or relative)

### Operations

#### Path Resolution

To resolve a path to an inode:

1. Start at root inode (ino=1)
2. Split path by `/` and filter empty components
3. For each component:
   ```sql
   SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?
   ```
4. Return final inode or NULL if any component not found

#### Creating a File

1. Resolve parent directory path to inode
2. Get chunk size from config:
   ```sql
   SELECT value FROM fs_config WHERE key = 'chunk_size'
   ```
3. Insert inode:
   ```sql
   INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
   VALUES (?, ?, ?, 0, ?, ?, ?)
   RETURNING ino
   ```
4. Insert directory entry:
   ```sql
   INSERT INTO fs_dentry (name, parent_ino, ino)
   VALUES (?, ?, ?)
   ```
5. Increment link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?
   ```
6. Split data into chunks and insert each:
   ```sql
   INSERT INTO fs_data (ino, chunk_index, data)
   VALUES (?, ?, ?)
   ```
   Where `chunk_index` starts at 0 and increments for each chunk.
7. Update inode size:
   ```sql
   UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?
   ```

#### Reading a File

1. Resolve path to inode
2. Fetch all chunks in order:
   ```sql
   SELECT data FROM fs_data WHERE ino = ? ORDER BY chunk_index ASC
   ```
3. Concatenate chunks in order
4. Update access time:
   ```sql
   UPDATE fs_inode SET atime = ? WHERE ino = ?
   ```

#### Reading a File at Offset

To read `length` bytes starting at byte offset `offset`:

1. Resolve path to inode
2. Get chunk size from config:
   ```sql
   SELECT value FROM fs_config WHERE key = 'chunk_size'
   ```
3. Calculate chunk range:
   - `start_chunk = offset / chunk_size`
   - `end_chunk = (offset + length - 1) / chunk_size`
4. Fetch required chunks:
   ```sql
   SELECT chunk_index, data FROM fs_data
   WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ?
   ORDER BY chunk_index ASC
   ```
5. Extract the requested byte range from the chunks:
   - `offset_in_first_chunk = offset % chunk_size`
   - Skip first `offset_in_first_chunk` bytes of first chunk
   - Take `length` total bytes across chunks

#### Listing a Directory

1. Resolve directory path to inode
2. Query entries:
   ```sql
   SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name ASC
   ```

#### Deleting a File

1. Resolve path to get inode and parent
2. Delete directory entry:
   ```sql
   DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?
   ```
3. Decrement link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?
   ```
4. Check if last link:
   ```sql
   SELECT nlink FROM fs_inode WHERE ino = ?
   ```
5. If nlink = 0, delete inode and data:
   ```sql
   DELETE FROM fs_inode WHERE ino = ?
   DELETE FROM fs_data WHERE ino = ?
   ```

#### Creating a Hard Link

1. Resolve source path to get inode
2. Resolve destination parent to get parent_ino
3. Insert new directory entry:
   ```sql
   INSERT INTO fs_dentry (name, parent_ino, ino)
   VALUES (?, ?, ?)
   ```
4. Increment link count:
   ```sql
   UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?
   ```

#### Reading File Metadata (stat)

1. Resolve path to inode
2. Query inode (includes link count):
   ```sql
   SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
          atime_nsec, mtime_nsec, ctime_nsec
   FROM fs_inode WHERE ino = ?
   ```

### Initialization

When creating a new agent database, initialize the filesystem configuration and root directory:

```sql
-- Initialize filesystem configuration
INSERT INTO fs_config (key, value) VALUES ('chunk_size', '4096');

-- Initialize root directory
INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
VALUES (1, 16877, 1, 0, 0, 0, unixepoch(), unixepoch(), unixepoch());
```

Where `16877` = `0o040755` (directory with rwxr-xr-x permissions)

**Note:** The `chunk_size` value can be customized at filesystem creation time but MUST NOT be changed afterward. The root directory has `nlink=1` as it has no parent directory entry.

### Consistency Rules

1. Root inode (ino=1) MUST always exist
2. Every dentry MUST reference a valid inode
3. Every dentry MUST reference a valid parent inode
4. No directory MAY contain duplicate names
5. Directories MUST have mode with S_IFDIR bit set
6. Regular files MUST have mode with S_IFREG bit set
7. File size MUST match total size of all data chunks
8. Every inode MUST have at least one dentry (except root)

### Implementation Notes

- Use `RETURNING` clause to safely get auto-generated inode numbers
- Parent directories are created implicitly as needed
- Empty files have an inode but no data chunks
- Symlink resolution is implementation-defined (not part of schema)
- Use transactions for multi-step operations to maintain consistency

### Extension Points

Implementations MAY extend the filesystem schema with additional functionality:

- Extended attributes table
- File ACLs and advanced permissions
- Quota tracking per user/group
- Version history and snapshots
- Content deduplication
- Compression metadata
- File checksums/hashes

Such extensions SHOULD use separate tables to maintain referential integrity.

## Overlay Filesystem

The overlay filesystem provides copy-on-write semantics by layering a writable delta filesystem on top of a read-only base filesystem. Changes are written to the delta layer while the base layer remains unmodified. This enables sandboxed execution where modifications can be discarded or committed independently.

### Whiteouts

When a file is deleted from an overlay filesystem, the deletion must be recorded so that lookups do not fall through to the base layer. This is accomplished using "whiteouts" - markers that indicate a path has been explicitly deleted.

#### Table: `fs_whiteout`

Tracks deleted paths in the overlay to prevent base layer visibility.

```sql
CREATE TABLE fs_whiteout (
  path TEXT PRIMARY KEY,
  parent_path TEXT NOT NULL,
  created_at INTEGER NOT NULL
)

CREATE INDEX idx_fs_whiteout_parent ON fs_whiteout(parent_path)
```

**Fields:**

- `path` - Normalized absolute path that has been deleted
- `parent_path` - Parent directory path (for efficient child lookups)
- `created_at` - Deletion timestamp (Unix timestamp, seconds)

**Notes:**

- The `parent_path` column enables O(1) lookups of whiteouts within a directory, avoiding expensive `LIKE` pattern matching
- For the root directory `/`, `parent_path` is `/`
- For other paths, `parent_path` is the path with the final component removed (e.g., `/foo/bar` has parent `/foo`)

### Operations

#### Create Whiteout

When deleting a file that exists in the base layer:

```sql
INSERT INTO fs_whiteout (path, parent_path, created_at)
VALUES (?, ?, ?)
ON CONFLICT(path) DO UPDATE SET created_at = excluded.created_at
```

#### Check for Whiteout

Before falling through to the base layer during lookup:

```sql
SELECT 1 FROM fs_whiteout WHERE path = ?
```

#### Remove Whiteout

When creating a file at a previously deleted path:

```sql
DELETE FROM fs_whiteout WHERE path = ?
```

#### List Child Whiteouts

When listing a directory, get whiteouts to exclude from base layer results:

```sql
SELECT path FROM fs_whiteout WHERE parent_path = ?
```

### Overlay Lookup Semantics

1. Check if path exists in delta layer → return delta entry
2. Check if path has a whiteout → return "not found"
3. Check if path exists in base layer → return base entry
4. Return "not found"

### Inode Origin Tracking

When a file is copied from the base layer to the delta layer during a copy-up operation (e.g., when creating a hard link to a base file), the original base inode number must be preserved. This is necessary because the kernel caches inode numbers, and returning a different inode after copy-up causes ENOENT errors or cache inconsistencies.

This mechanism is similar to Linux overlayfs's `trusted.overlay.origin` extended attribute, which stores a file handle to the lower inode.

#### Table: `fs_origin`

Maps delta layer inodes to their original base layer inodes.

```sql
CREATE TABLE fs_origin (
  delta_ino INTEGER PRIMARY KEY,
  base_ino INTEGER NOT NULL
)
```

**Fields:**

- `delta_ino` - Inode number in the delta layer
- `base_ino` - Original inode number from the base layer

#### Operations

##### Store Origin Mapping

When copying a file from base to delta during copy-up:

```sql
INSERT OR REPLACE INTO fs_origin (delta_ino, base_ino)
VALUES (?, ?)
```

##### Get Origin Inode

When stat'ing a file that exists in delta, check if it has an origin:

```sql
SELECT base_ino FROM fs_origin WHERE delta_ino = ?
```

If a mapping exists, return `base_ino` instead of `delta_ino` in stat results.

### Consistency Rules

1. A whiteout MUST be removed when a new file is created at that path
2. A whiteout MUST be created when deleting a file that exists in the base layer
3. The `parent_path` MUST be correctly derived from `path`
4. Whiteouts only affect overlay lookups, not the underlying base filesystem
5. When copying a file from base to delta, the origin mapping MUST be stored
6. When stat'ing a delta file with an origin mapping, the base inode MUST be returned

## Key-Value Data

The key-value store provides simple get/set operations for agent context and state.

### Schema

#### Table: `kv_store`

Stores arbitrary key-value pairs with automatic timestamping.

```sql
CREATE TABLE kv_store (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  created_at INTEGER DEFAULT (unixepoch()),
  updated_at INTEGER DEFAULT (unixepoch())
)

CREATE INDEX idx_kv_store_created_at ON kv_store(created_at)
```

**Fields:**

- `key` - Unique key identifier
- `value` - JSON-serialized value
- `created_at` - Creation timestamp (Unix timestamp, seconds)
- `updated_at` - Last update timestamp (Unix timestamp, seconds)

### Operations

#### Set a Value

```sql
INSERT INTO kv_store (key, value, updated_at)
VALUES (?, ?, unixepoch())
ON CONFLICT(key) DO UPDATE SET
  value = excluded.value,
  updated_at = unixepoch()
```

#### Get a Value

```sql
SELECT value FROM kv_store WHERE key = ?
```

#### Delete a Value

```sql
DELETE FROM kv_store WHERE key = ?
```

#### List All Keys

```sql
SELECT key, created_at, updated_at FROM kv_store ORDER BY key ASC
```

### Consistency Rules

1. Keys MUST be unique (enforced by PRIMARY KEY)
2. Values MUST be valid JSON strings
3. Timestamps MUST use Unix epoch format (seconds)

### Implementation Notes

- Values are stored as JSON strings; serialize before storing, deserialize after retrieving
- Use `ON CONFLICT` clause for upsert operations
- Indexes on `created_at` support temporal queries
- Updates automatically refresh the `updated_at` timestamp
- Keys can use any naming convention (e.g., namespaced: `user:preferences`, `session:state`)

### Extension Points

Implementations MAY extend the key-value store schema with additional functionality:

- Namespaced keys with hierarchy support
- Value versioning/history
- TTL (time-to-live) for automatic expiration
- Value size limits and quotas

Such extensions SHOULD use separate tables to maintain referential integrity.

## Revision History

### Version 0.4

- Added nanosecond timestamp precision for `atime`, `mtime`, and `ctime`
- Added `atime_nsec`, `mtime_nsec`, `ctime_nsec` columns to `fs_inode` table (DEFAULT 0 for backward compatibility)
- Nanosecond precision enables correct NFS `wcc_data` cache invalidation when multiple operations occur within the same second
- Added POSIX special file support (FIFOs, character devices, block devices, sockets)
- Added `rdev` column to `fs_inode` table for device major/minor numbers
- Added `S_IFIFO`, `S_IFCHR`, `S_IFBLK`, `S_IFSOCK` file type constants to Mode Encoding
- Updated stat query to include `rdev` field

### Version 0.3

- Added `fs_origin` table to Overlay Filesystem for tracking copy-up origin inodes
- Origin tracking ensures consistent inode numbers after copy-up (similar to Linux overlayfs `trusted.overlay.origin`)

### Version 0.2

- Added Overlay Filesystem section with `fs_whiteout` table for copy-on-write semantics
- Whiteout table includes `parent_path` column with index for efficient O(1) child lookups
- Added `nlink` column to `fs_inode` table to store link count directly
- Link count is now maintained in the inode rather than computed via COUNT(*) on `fs_dentry`

### Version 0.1

- Added `fs_config` table for filesystem-level configuration
- Changed `fs_data` table to use fixed-size chunks with `chunk_index` instead of variable-size chunks with `offset` and `size`
- Added `chunk_size` configuration option (default: 4096 bytes)
- Added "Reading a File at Offset" operation for efficient partial reads
- Chunk-based storage enables efficient random access reads without loading entire files

### Version 0.0

- Initial specification
- Tool call audit trail (`tool_calls` table)
- Virtual filesystem (`fs_inode`, `fs_dentry`, `fs_data`, `fs_symlink` tables)
- Key-value store (`kv_store` table)
