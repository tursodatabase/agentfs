# AgentFS Specification Testable Elements

This document captures the testable elements from the AgentFS Specification (SPEC.md)
and defines the testing strategy for ensuring spec compliance.

**Current Spec Version:** 0.4

---

## Testing Strategy

### Approach: Dedicated Spec Compliance Suite

We maintain a dedicated `spec_test.go` focused exclusively on spec compliance, separate
from existing implementation tests.

| Approach | Pros | Cons |
|----------|------|------|
| **Dedicated suite** | Clear traceability, easy auditing, spec changes are isolated | Some duplication |
| **Interspersed** | No duplication | Hard to audit, spec mapping scattered, harder to maintain |

### File Organization

```
sdk/go/
├── spec_test.go           # Spec compliance tests
├── agentfs_test.go        # Implementation/behavior tests
├── overlay_test.go        # Overlay implementation tests
├── symlink_test.go        # Symlink resolution tests
└── SPEC_TESTS.md          # Spec element tracking (this file)
```

### Design Principles for `spec_test.go`

1. **Test names include spec IDs** for traceability:
   ```go
   func TestSpec_FS24_RootInodeMustExist(t *testing.T)
   func TestSpec_OV18_WhiteoutRemovedOnCreate(t *testing.T)
   func TestSpec_KV13_KeysMustBeUnique(t *testing.T)
   ```

2. **Organized by spec section** (Tool Calls, Virtual Filesystem, Overlay, Key-Value)

3. **Minimal tests** - just enough to verify compliance, not exhaustive edge cases

4. **Schema tests verify structure** by querying sqlite_master

5. **Version-tagged** for tracking which spec version introduced each element

### Test File Responsibilities

| Test File | Purpose | Scope |
|-----------|---------|-------|
| `spec_test.go` | Spec compliance | Minimal verification of each spec element |
| `agentfs_test.go` | Implementation | Detailed behavior, edge cases, integration |
| `overlay_test.go` | Overlay behavior | Copy-up, caching, complex scenarios |
| `symlink_test.go` | Symlink resolution | Follow, ELOOP, intermediate traversal |

### Running Spec Tests

```bash
# Run only spec compliance tests
go test -v -run TestSpec ./...

# Run specific section
go test -v -run TestSpec_FS ./...   # Filesystem
go test -v -run TestSpec_OV ./...   # Overlay
go test -v -run TestSpec_KV ./...   # Key-Value
go test -v -run TestSpec_TC ./...   # Tool Calls
```

### Workflow for Spec Changes

```
1. Spec updated (SPEC.md)
        ↓
2. Update SPEC_TESTS.md with new elements (this file)
        ↓
3. Add tests to spec_test.go with spec IDs
        ↓
4. Update Status column in SPEC_TESTS.md to ✓
```

### Status Legend

| Status | Meaning |
|--------|---------|
| ✓ | Test exists in spec_test.go |
| ~ | Covered by implementation tests (not spec_test.go) |
| (empty) | Not yet tested |

---

## 1. Tool Calls

### Schema Constraints

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| TC-1 | Table `tool_calls` has all required columns with correct types | 0.0 | |
| TC-2 | Index `idx_tool_calls_name` exists on `name` column | 0.0 | |
| TC-3 | Index `idx_tool_calls_started_at` exists on `started_at` column | 0.0 | |
| TC-4 | `id` is auto-incrementing primary key | 0.0 | |

### Consistency Rules

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| TC-5 | Exactly one of `result` or `error` SHOULD be non-NULL (mutual exclusion) | 0.0 | |
| TC-6 | `completed_at` MUST always be set (no NULL values) | 0.0 | |
| TC-7 | `duration_ms` MUST always be set and equal to `(completed_at - started_at) * 1000` | 0.0 | |
| TC-8 | `parameters` MUST be valid JSON string when present | 0.0 | |
| TC-9 | `result` MUST be valid JSON string when present | 0.0 | |
| TC-10 | Records MUST NOT be updated after insertion (insert-only audit log) | 0.0 | |
| TC-11 | Records MUST NOT be deleted (insert-only audit log) | 0.0 | |

### Operations

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| TC-12 | Record tool call inserts correctly with all fields | 0.0 | |
| TC-13 | Query by name returns correct records ordered by `started_at DESC` | 0.0 | |
| TC-14 | Query recent tool calls filters by `started_at` correctly | 0.0 | |
| TC-15 | Performance analysis query groups by name with correct counts | 0.0 | |

---

## 2. Virtual Filesystem

### Schema Constraints

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-1 | Table `fs_config` exists with `key` TEXT PRIMARY KEY, `value` TEXT NOT NULL | 0.1 | |
| FS-2 | Table `fs_inode` has columns: ino, mode, nlink, uid, gid, size, atime, mtime, ctime | 0.0 | |
| FS-3 | Table `fs_inode` has `nlink` column for link count | 0.2 | |
| FS-4 | Table `fs_inode` has `rdev` column for device numbers | 0.4 | |
| FS-5 | Table `fs_inode` has nanosecond columns: atime_nsec, mtime_nsec, ctime_nsec | 0.4 | |
| FS-6 | Table `fs_dentry` has columns (id, name, parent_ino, ino) with UNIQUE(parent_ino, name) | 0.0 | |
| FS-7 | Index `idx_fs_dentry_parent` exists on (parent_ino, name) | 0.0 | |
| FS-8 | Table `fs_data` has PRIMARY KEY (ino, chunk_index) | 0.1 | |
| FS-9 | Table `fs_symlink` has `ino` INTEGER PRIMARY KEY, `target` TEXT NOT NULL | 0.0 | |

### Configuration

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-10 | `chunk_size` configuration exists with default value 4096 | 0.1 | |
| FS-11 | Configuration is immutable after initialization | 0.1 | |

### Mode Encoding

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-12 | S_IFMT (0o170000) correctly masks file type | 0.0 | |
| FS-13 | S_IFREG (0o100000) identifies regular files | 0.0 | |
| FS-14 | S_IFDIR (0o040000) identifies directories | 0.0 | |
| FS-15 | S_IFLNK (0o120000) identifies symbolic links | 0.0 | |
| FS-16 | S_IFIFO (0o010000) identifies FIFOs/named pipes | 0.4 | |
| FS-17 | S_IFCHR (0o020000) identifies character devices | 0.4 | |
| FS-18 | S_IFBLK (0o060000) identifies block devices | 0.4 | |
| FS-19 | S_IFSOCK (0o140000) identifies sockets | 0.4 | |
| FS-20 | Permission bits (0o000777) correctly encode rwxrwxrwx | 0.0 | |

### Initialization

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-21 | Root inode (ino=1) is created on initialization | 0.0 | |
| FS-22 | Root inode has mode 0o040755 (directory with rwxr-xr-x) | 0.0 | |
| FS-23 | Root inode has nlink=1 | 0.2 | |

### Consistency Rules

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-24 | Root inode (ino=1) MUST always exist | 0.0 | |
| FS-25 | Every dentry MUST reference a valid inode | 0.0 | |
| FS-26 | Every dentry MUST reference a valid parent inode | 0.0 | |
| FS-27 | No directory MAY contain duplicate names | 0.0 | |
| FS-28 | Directories MUST have mode with S_IFDIR bit set | 0.0 | |
| FS-29 | Regular files MUST have mode with S_IFREG bit set | 0.0 | |
| FS-30 | File size MUST match total size of all data chunks | 0.0 | |
| FS-31 | Every inode MUST have at least one dentry (except root) | 0.0 | |

### Operations - Path Resolution

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-32 | Path resolution starts at root inode (ino=1) | 0.0 | |
| FS-33 | Path components are split by `/` with empty components filtered | 0.0 | |
| FS-34 | Each component lookup uses parent_ino and name | 0.0 | |
| FS-35 | Missing component returns NULL/not found | 0.0 | |

### Operations - File Creation

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-36 | Creating file resolves parent directory first | 0.0 | |
| FS-37 | New inode is inserted with correct mode, uid, gid, timestamps | 0.0 | |
| FS-38 | Directory entry is created linking name to inode | 0.0 | |
| FS-39 | Link count is incremented on inode | 0.2 | |
| FS-40 | Data is split into chunks of chunk_size | 0.1 | |
| FS-41 | Inode size is updated to match content | 0.0 | |

### Operations - File Reading

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-42 | Reading file resolves path to inode | 0.0 | |
| FS-43 | All chunks are fetched in order by chunk_index ASC | 0.1 | |
| FS-44 | Chunks are concatenated correctly | 0.1 | |
| FS-45 | Access time (atime) is updated on read | 0.0 | |

### Operations - Partial File Reading

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-46 | start_chunk = offset / chunk_size | 0.1 | |
| FS-47 | end_chunk = (offset + length - 1) / chunk_size | 0.1 | |
| FS-48 | Only required chunks are fetched | 0.1 | |
| FS-49 | offset_in_first_chunk = offset % chunk_size | 0.1 | |
| FS-50 | Correct byte range is extracted | 0.1 | |

### Operations - Directory Listing

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-51 | Readdir returns names from fs_dentry for parent_ino | 0.0 | |
| FS-52 | Results are ordered by name ASC | 0.0 | |

### Operations - File Deletion

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-53 | Deleting file removes directory entry | 0.0 | |
| FS-54 | Link count is decremented | 0.2 | |
| FS-55 | When nlink reaches 0, inode is deleted | 0.2 | |
| FS-56 | When nlink reaches 0, data chunks are deleted | 0.0 | |

### Operations - Hard Links

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-57 | Hard link creates new dentry pointing to same inode | 0.0 | |
| FS-58 | Link count is incremented on target inode | 0.2 | |
| FS-59 | Multiple dentries can point to same inode | 0.0 | |

### Operations - Stat

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-60 | Stat returns all inode fields (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev) | 0.4 | |
| FS-61 | Stat returns nanosecond fields (atime_nsec, mtime_nsec, ctime_nsec) | 0.4 | |

### Chunk Storage

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-62 | Directories MUST NOT have data chunks | 0.0 | |
| FS-63 | All chunks except last MUST be exactly chunk_size bytes | 0.1 | |
| FS-64 | Last chunk MAY be smaller than chunk_size | 0.1 | |
| FS-65 | Byte offset = chunk_index * chunk_size | 0.1 | |
| FS-66 | Empty files have inode but no data chunks | 0.0 | |

### Symlinks

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| FS-67 | Symlink inode is stored in fs_symlink table | 0.0 | |
| FS-68 | Symlink target can be absolute or relative path | 0.0 | |
| FS-69 | Symlink has mode with S_IFLNK bit set | 0.0 | |

---

## 3. Overlay Filesystem

### Schema Constraints

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| OV-1 | Table `fs_whiteout` has `path` TEXT PRIMARY KEY | 0.2 | |
| OV-2 | Table `fs_whiteout` has `parent_path` TEXT NOT NULL | 0.2 | |
| OV-3 | Table `fs_whiteout` has `created_at` INTEGER NOT NULL | 0.2 | |
| OV-4 | Index `idx_fs_whiteout_parent` exists on `parent_path` | 0.2 | |
| OV-5 | Table `fs_origin` has `delta_ino` INTEGER PRIMARY KEY | 0.3 | |
| OV-6 | Table `fs_origin` has `base_ino` INTEGER NOT NULL | 0.3 | |

### Whiteout Operations

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| OV-7 | Create whiteout inserts path with parent_path and created_at | 0.2 | |
| OV-8 | Check whiteout returns true for existing whiteout path | 0.2 | |
| OV-9 | Remove whiteout deletes the path entry | 0.2 | |
| OV-10 | List child whiteouts returns paths with matching parent_path | 0.2 | |

### Overlay Lookup Semantics

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| OV-11 | Lookup returns delta entry if path exists in delta | 0.2 | |
| OV-12 | Lookup returns "not found" if path has whiteout | 0.2 | |
| OV-13 | Lookup returns base entry if path exists only in base | 0.2 | |
| OV-14 | Lookup returns "not found" if path doesn't exist in either layer | 0.2 | |

### Origin Tracking Operations

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| OV-15 | Store origin mapping inserts delta_ino → base_ino | 0.3 | |
| OV-16 | Get origin returns base_ino for delta_ino if mapping exists | 0.3 | |
| OV-17 | Origin mapping uses INSERT OR REPLACE semantics | 0.3 | |

### Consistency Rules

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| OV-18 | Whiteout MUST be removed when new file created at that path | 0.2 | |
| OV-19 | Whiteout MUST be created when deleting file from base layer | 0.2 | |
| OV-20 | parent_path MUST be correctly derived from path | 0.2 | |
| OV-21 | Whiteouts only affect overlay lookups, not base filesystem | 0.2 | |
| OV-22 | Origin mapping MUST be stored when copying from base to delta | 0.3 | |
| OV-23 | Base inode MUST be returned when stat'ing delta file with origin | 0.3 | |

### Parent Path Derivation

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| OV-24 | Root directory `/` has parent_path `/` | 0.2 | |
| OV-25 | Path `/foo/bar` has parent_path `/foo` | 0.2 | |
| OV-26 | Path `/foo` has parent_path `/` | 0.2 | |

---

## 4. Key-Value Store

### Schema Constraints

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| KV-1 | Table `kv_store` has `key` TEXT PRIMARY KEY | 0.0 | |
| KV-2 | Table `kv_store` has `value` TEXT NOT NULL | 0.0 | |
| KV-3 | Table `kv_store` has `created_at` with DEFAULT (unixepoch()) | 0.0 | |
| KV-4 | Table `kv_store` has `updated_at` with DEFAULT (unixepoch()) | 0.0 | |
| KV-5 | Index `idx_kv_store_created_at` exists on `created_at` | 0.0 | |

### Operations

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| KV-6 | Set inserts new key with value, created_at, updated_at | 0.0 | |
| KV-7 | Set updates existing key's value and updated_at (upsert) | 0.0 | |
| KV-8 | Set preserves created_at on update | 0.0 | |
| KV-9 | Get returns value for existing key | 0.0 | |
| KV-10 | Get returns NULL/not found for missing key | 0.0 | |
| KV-11 | Delete removes key | 0.0 | |
| KV-12 | List returns all keys ordered by key ASC | 0.0 | |

### Consistency Rules

| ID | Testable Element | Version | Status |
|----|------------------|---------|--------|
| KV-13 | Keys MUST be unique | 0.0 | |
| KV-14 | Values MUST be valid JSON strings | 0.0 | |
| KV-15 | Timestamps MUST use Unix epoch format (seconds) | 0.0 | |

---

## Summary

| Section | Count | Versions |
|---------|-------|----------|
| Tool Calls | 15 | 0.0 |
| Virtual Filesystem | 69 | 0.0, 0.1, 0.2, 0.4 |
| Overlay Filesystem | 26 | 0.2, 0.3 |
| Key-Value Store | 15 | 0.0 |
| **Total** | **125** | |

---

## Version History

| Version | Changes |
|---------|---------|
| 0.4 | Nanosecond timestamp precision (atime_nsec, mtime_nsec, ctime_nsec), POSIX special files (rdev, S_IFIFO, S_IFCHR, S_IFBLK, S_IFSOCK) |
| 0.3 | fs_origin table for overlay copy-up origin tracking |
| 0.2 | fs_whiteout table with parent_path, nlink column in fs_inode |
| 0.1 | fs_config table, chunk-based storage with chunk_index |
| 0.0 | Initial specification (tool_calls, fs_inode, fs_dentry, fs_data, fs_symlink, kv_store) |

---

## Test Coverage Mapping

Tests for these elements can be found in:

- `agentfs_test.go` - Core filesystem operations
- `overlay_test.go` - Overlay filesystem operations
- `symlink_test.go` - Symlink resolution behavior

### Overlay Consistency Rules (OV-18 to OV-23)

These critical consistency rules have dedicated tests:

| Rule | Test Function |
|------|---------------|
| OV-18 | `TestOverlay_ConsistencyRule1_WhiteoutRemovedOnCreate` |
| OV-19 | `TestOverlay_ConsistencyRule2_WhiteoutCreatedOnDelete` |
| OV-20 | `TestOverlay_ConsistencyRule3_ParentPathCorrectlyDerived` |
| OV-21 | `TestOverlay_ConsistencyRule4_WhiteoutsAffectOverlayLookups` |
| OV-22 | `TestOverlay_ConsistencyRule5_OriginMappingStored` |
| OV-23 | `TestOverlay_ConsistencyRule6_BaseInodeReturnedForCopiedUp` |
