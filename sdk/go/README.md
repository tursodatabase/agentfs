# AgentFS Go SDK

A Go SDK for AgentFS - a SQLite-backed virtual filesystem, key-value store, and tool call tracking system for AI agents.

## Installation

```bash
go get github.com/tursodatabase/agentfs/sdk/go
```

## Quick Start

```go
package main

import (
	"context"
	"fmt"
	"log"
	"time"

	agentfs "github.com/tursodatabase/agentfs/sdk/go"
)

func main() {
	ctx := context.Background()

	// Open or create an AgentFS database
	afs, err := agentfs.Open(ctx, agentfs.AgentFSOptions{
		ID: "my-agent", // Creates ~/.agentfs/my-agent.db
	})
	if err != nil {
		log.Fatal(err)
	}
	defer afs.Close()

	// === Filesystem Operations ===

	// Write a file
	err = afs.FS.WriteFile(ctx, "/hello.txt", []byte("Hello, World!"), 0o644)
	if err != nil {
		log.Fatal(err)
	}

	// Read a file
	data, err := afs.FS.ReadFile(ctx, "/hello.txt")
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println(string(data)) // "Hello, World!"

	// Create directories
	err = afs.FS.MkdirAll(ctx, "/path/to/dir", 0o755)

	// List directory
	names, err := afs.FS.Readdir(ctx, "/")
	for _, name := range names {
		fmt.Printf("File name: %s\n", name)
	}

	// Get file stats
	stats, err := afs.FS.Stat(ctx, "/hello.txt")
	fmt.Printf("Size: %d, IsDir: %v\n", stats.Size, stats.IsDir())

	// === Key-Value Store ===

	// Store values (JSON-serialized)
	err = afs.KV.Set(ctx, "config:version", "1.0.0")

	type Settings struct {
		Theme    string `json:"theme"`
		FontSize int    `json:"font_size"`
	}
	err = afs.KV.Set(ctx, "user:settings", Settings{Theme: "dark", FontSize: 14})

	// Retrieve values
	var version string
	err = afs.KV.Get(ctx, "config:version", &version)

	var settings Settings
	err = afs.KV.Get(ctx, "user:settings", &settings)

	// List keys by prefix
	keys, err := afs.KV.Keys(ctx, "config:")
	for _, key := range keys {
		fmt.Printf("Key: %s\n", key)
	}

	// === Tool Call Tracking ===

	// Start/Success pattern
	pending, err := afs.Tools.Start(ctx, "web_search", map[string]string{
		"query": "golang sqlite",
	})

	// ... perform operation ...

	call, err := pending.Success(ctx, map[string]any{
		"results": []string{"result1", "result2"},
	})
	fmt.Printf("Tool call %d completed in %dms\n", call.ID, call.DurationMs)

	// Or record directly
	now := time.Now().Unix()
	call, err = afs.Tools.Record(ctx, "read_file",
		map[string]string{"path": "/test.txt"},
		"file contents",
		nil, // no error
		now-1, now,
	)

	// Query tool calls
	calls, err := afs.Tools.GetByName(ctx, "web_search", 10)
	stats, err := afs.Tools.GetStats(ctx)
}
```

## API Reference

### AgentFS

```go
// Open creates or opens an AgentFS database
func Open(ctx context.Context, opts AgentFSOptions) (*AgentFS, error)

type AgentFSOptions struct {
    ID        string       // Agent ID (creates ~/.agentfs/{id}.db)
    Path      string       // Explicit database path (takes precedence)
    ChunkSize int          // Chunk size for file data (default: 4096)
    Cache     CacheOptions // Optional path resolution cache
    Pool      PoolOptions  // Connection pool configuration
}

type CacheOptions struct {
    Enabled    bool          // Enable LRU cache (default: false)
    MaxEntries int           // Max cached paths (default: 10000)
    TTL        time.Duration // Entry expiration (0 = no expiration)
}

type PoolOptions struct {
    MaxOpenConns    int           // Max open connections (0 = unlimited)
    MaxIdleConns    int           // Max idle connections (default: 2)
    ConnMaxLifetime time.Duration // Max connection lifetime (0 = forever)
    ConnMaxIdleTime time.Duration // Max idle time (0 = forever)
}
```

### Filesystem

| Method                        | Description                   |
|-------------------------------|-------------------------------|
| `Stat(path)`                  | Get file/directory metadata   |
| `Readdir(path)`               | List directory entries        |
| `ReaddirPlus(path)`           | List entries with stats       |
| `Mkdir(path, mode)`           | Create directory              |
| `MkdirAll(path, mode)`        | Create directory and parents  |
| `ReadFile(path)`              | Read entire file              |
| `WriteFile(path, data, mode)` | Write file (creates parents)  |
| `Unlink(path)`                | Delete file                   |
| `Rmdir(path)`                 | Delete empty directory        |
| `Rename(old, new)`            | Move/rename file or directory |
| `Link(existing, new)`         | Create hard link              |
| `Symlink(target, link)`       | Create symbolic link          |
| `Readlink(path)`              | Read symlink target           |
| `Chmod(path, mode)`           | Change permissions            |
| `Utimes(path, atime, mtime)`  | Update timestamps (seconds)   |
| `UtimesNano(path, ...)`       | Update timestamps (nanoseconds) |
| `Open(path, flags)`           | Open file handle              |
| `Create(path, mode)`          | Create new file handle        |

### File Handle

The `File` type implements Go's standard I/O interfaces for seamless integration:

```go
// File implements:
//   - io.Reader, io.Writer (sequential I/O)
//   - io.ReaderAt, io.WriterAt (random access)
//   - io.Seeker, io.Closer

// Sequential I/O (tracks position automatically)
f, _ := afs.FS.Open(ctx, "/file.txt", agentfs.O_RDWR)
defer f.Close()

data, _ := io.ReadAll(f)           // Read entire file
f.Seek(0, io.SeekStart)            // Seek back to start
f.Write([]byte("new content"))     // Write at current position

// Random access (does not affect position)
f.ReadAt(buf, 100)                 // Read at offset 100
f.WriteAt(data, 200)               // Write at offset 200

// Context-aware positioned I/O
f.Pread(ctx, buf, offset)          // Read with context
f.Pwrite(ctx, data, offset)        // Write with context

// Use with io.Copy
io.Copy(destFile, srcFile)         // Copy between files
io.Copy(f, bytes.NewReader(data))  // Write from bytes.Reader
```

| Method                      | Description                    |
|-----------------------------|--------------------------------|
| `Read(p []byte)`            | Read sequentially (io.Reader)  |
| `Write(p []byte)`           | Write sequentially (io.Writer) |
| `Seek(offset, whence)`      | Set position (io.Seeker)       |
| `ReadAt(p, offset)`         | Read at offset (io.ReaderAt)   |
| `WriteAt(p, offset)`        | Write at offset (io.WriterAt)  |
| `Pread(ctx, buf, offset)`   | Positioned read with context   |
| `Pwrite(ctx, data, offset)` | Positioned write with context  |
| `Truncate(ctx, size)`       | Set file size                  |
| `Stat(ctx)`                 | Get metadata                   |
| `Size()`                    | Get current file size          |
| `Offset()`                  | Get current position           |
| `Close()`                   | Close handle                   |

### Stats Struct

The `Stats` struct returned by `Stat()` and `ReaddirPlus()` includes nanosecond-precision timestamps (SPEC v0.4):

```go
type Stats struct {
    Ino       int64  // Inode number
    Mode      int64  // File type and permissions
    Nlink     int64  // Number of hard links
    UID       int64  // Owner user ID
    GID       int64  // Owner group ID
    Size      int64  // File size in bytes
    Atime     int64  // Last access time (seconds)
    Mtime     int64  // Last modification time (seconds)
    Ctime     int64  // Change time (seconds)
    Rdev      int64  // Device number (for special files)
    AtimeNsec int64  // Nanosecond component of atime (0-999999999)
    MtimeNsec int64  // Nanosecond component of mtime (0-999999999)
    CtimeNsec int64  // Nanosecond component of ctime (0-999999999)
}

// Helper methods for full time.Time values
stats.AtimeTime()  // Returns time.Time with nanosecond precision
stats.MtimeTime()  // Returns time.Time with nanosecond precision
stats.CtimeTime()  // Returns time.Time with nanosecond precision

// File type helpers
stats.IsDir()          // Is directory?
stats.IsRegularFile()  // Is regular file?
stats.IsSymlink()      // Is symbolic link?
stats.IsFIFO()         // Is FIFO/named pipe?
stats.IsCharDevice()   // Is character device?
stats.IsBlockDevice()  // Is block device?
stats.IsSocket()       // Is socket?
```

### Key-Value Store

| Method            | Description                        |
|-------------------|------------------------------------|
| `Set(key, value)` | Store value (JSON-serialized)      |
| `Get(key, dest)`  | Retrieve and unmarshal value       |
| `GetRaw(key)`     | Get raw JSON value                 |
| `Delete(key)`     | Remove key                         |
| `Has(key)`        | Check if key exists                |
| `Keys(prefix)`    | List keys (optionally by prefix)   |
| `List(prefix)`    | List keys with metadata            |
| `Clear(prefix)`   | Delete keys (optionally by prefix) |

#### Generic Helper Functions (Go 1.18+)

The SDK provides type-safe generic functions for cleaner KV operations:

```go
// Type-safe get - returns value directly instead of requiring dest pointer
version, err := agentfs.KVGet[string](ctx, afs.KV, "config:version")
count, err := agentfs.KVGet[int](ctx, afs.KV, "stats:count")

type Config struct {
    Debug bool `json:"debug"`
}
cfg, err := agentfs.KVGet[Config](ctx, afs.KV, "app:config")

// Get with default value - returns default if key doesn't exist
debug, err := agentfs.KVGetOrDefault(ctx, afs.KV, "config:debug", false)
name, err := agentfs.KVGetOrDefault(ctx, afs.KV, "user:name", "anonymous")

// Get with zero value - returns zero value if key doesn't exist
count, err := agentfs.KVGetOrZero[int](ctx, afs.KV, "stats:count")    // 0 if not found
name, err := agentfs.KVGetOrZero[string](ctx, afs.KV, "user:name")    // "" if not found

// Type-safe set (for API consistency)
err := agentfs.KVSet(ctx, afs.KV, "config:version", "1.0.0")
```

| Function                                   | Description                             |
|--------------------------------------------|-----------------------------------------|
| `KVGet[T](ctx, kv, key)`                   | Type-safe get, returns value directly   |
| `KVGetOrDefault[T](ctx, kv, key, default)` | Returns default if key not found        |
| `KVGetOrZero[T](ctx, kv, key)`             | Returns zero value if key not found     |
| `KVSet[T](ctx, kv, key, value)`            | Type-safe set (wrapper for consistency) |

### Tool Calls

| Method                        | Description               |
|-------------------------------|---------------------------|
| `Start(name, params)`         | Begin tracking a call     |
| `PendingCall.Success(result)` | Mark as successful        |
| `PendingCall.Error(err)`      | Mark as failed            |
| `Record(...)`                 | Insert complete record    |
| `Get(id)`                     | Get call by ID            |
| `GetByName(name, limit)`      | Get calls by name         |
| `GetRecent(since, limit)`     | Get recent calls          |
| `GetStats()`                  | Get aggregated statistics |

## Error Handling

The SDK uses POSIX-style error codes:

```go
import "errors"

_, err := afs.FS.Stat(ctx, "/missing")
if agentfs.IsNotExist(err) {
    // Handle missing file
}

var fsErr *agentfs.FSError
if errors.As(err, &fsErr) {
    fmt.Printf("Code: %d, Syscall: %s, Path: %s\n",
        fsErr.Code, fsErr.Syscall, fsErr.Path)
}
```

Common error codes:
- `ENOENT` (2) - No such file or directory
- `EEXIST` (17) - File exists
- `ENOTDIR` (20) - Not a directory
- `EISDIR` (21) - Is a directory
- `ENOTEMPTY` (39) - Directory not empty

## Interfaces for Testing

The SDK provides optional interfaces for users who want to mock the filesystem, KV store, or tool calls in their tests:

```go
// Your application code - program against the interface
type MyService struct {
    fs agentfs.FileSystem  // Interface, not *Filesystem
}

func NewMyService(afs *agentfs.AgentFS) *MyService {
    return &MyService{fs: afs.FS}  // Concrete type satisfies interface
}

// In tests - use a mock
type MockFS struct{}
func (m *MockFS) Stat(ctx context.Context, path string) (*agentfs.Stats, error) {
    return &agentfs.Stats{Size: 100}, nil
}
// ... implement other methods

svc := &MyService{fs: &MockFS{}}
```

| Interface            | Concrete Type |
|----------------------|---------------|
| `FileSystem`         | `*Filesystem` |
| `KVStoreInterface`   | `*KVStore`    |
| `ToolCallsInterface` | `*ToolCalls`  |

These interfaces are entirely optional. The SDK continues to return concrete types, and users who don't need mocking can ignore the interfaces entirely.

## Go Standard Library `io/fs` Integration

The SDK provides an `io/fs` compatible wrapper for use with Go standard library functions:

```go
import (
    "io/fs"
    "html/template"
    "net/http"
)

// Create io/fs wrapper
iofs := agentfs.NewIOFS(afs.FS)

// Use with fs.WalkDir
fs.WalkDir(iofs, ".", func(path string, d fs.DirEntry, err error) error {
    fmt.Println(path)
    return nil
})

// Use with fs.Glob
matches, _ := fs.Glob(iofs, "*.txt")

// Use with template.ParseFS
tmpl, _ := template.ParseFS(iofs, "templates/*.html")

// Use with http.FileServer
http.Handle("/", http.FileServer(http.FS(iofs)))

// Use SubFS for subdirectories
subFS, _ := iofs.Sub("public")
http.Handle("/static/", http.StripPrefix("/static/", http.FileServer(http.FS(subFS))))

// Use with context for timeout control
ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
defer cancel()
iofsWithCtx := iofs.WithContext(ctx)
```

### Supported Interfaces

| Interface        | Status                      |
|------------------|-----------------------------|
| `fs.FS`          | Supported                   |
| `fs.StatFS`      | Supported                   |
| `fs.ReadFileFS`  | Supported                   |
| `fs.ReadDirFS`   | Supported                   |
| `fs.SubFS`       | Supported                   |
| `fs.ReadDirFile` | Supported (for directories) |

**Note:** Use `fs.Glob(iofs, pattern)` for glob matching - it uses the `ReadDir` implementation.

## Benchmarks

Run benchmarks with:

```bash
go test -bench=. -run=^$ -benchmem
```

### Sequential I/O (1MB files)

| Operation       | Throughput | Allocs/op |
|-----------------|------------|-----------|
| WriteFile       | 274 MB/s   | 1,898     |
| ReadFile        | 752 MB/s   | 1,101     |
| Streaming Write | 93 MB/s    | 13,628    |
| Streaming Read  | 167 MB/s   | 12,612    |

### Random I/O (16MB file, 4KB operations)

| Operation   | Throughput |
|-------------|------------|
| RandomRead  | 138 MB/s   |
| RandomWrite | 56 MB/s    |

### Metadata Operations

| Operation             | Latency |
|-----------------------|---------|
| Stat                  | 13 μs   |
| Readdir (100 entries) | 57 μs   |
| Mkdir                 | 60 μs   |
| Rename                | 24 μs   |
| Create+Delete         | 132 μs  |

### Path Resolution

| Depth     | Latency |
|-----------|---------|
| 1 level   | 18 μs   |
| 10 levels | 60 μs   |
| 20 levels | 106 μs  |

### Chunk Size Impact (1MB file)

| Chunk Size    | Write    | Read     |
|---------------|----------|----------|
| 4KB (default) | 266 MB/s | 740 MB/s |
| 16KB          | 491 MB/s | 644 MB/s |
| 64KB          | 947 MB/s | 842 MB/s |

### KV Store

| Operation         | Latency |
|-------------------|---------|
| Get (small value) | 4 μs    |
| Set (small value) | 16 μs   |
| Has               | 3 μs    |
| Keys (100 keys)   | 56 μs   |

### Path Resolution Cache

| Depth     | Without Cache | With Cache | Speedup |
|-----------|---------------|------------|---------|
| 1 level   | 18 μs         | 9 μs       | 2x      |
| 10 levels | 60 μs         | 9 μs       | 6.7x    |
| 20 levels | 108 μs        | 9 μs       | 12x     |

### Key Insights

- **Chunk size matters**: Larger chunks significantly improve write performance (3.5x from 4KB to 64KB). Consider using `ChunkSize: 65536` for write-heavy workloads.
- **Path resolution cache eliminates depth penalty**: With caching enabled, deep paths perform the same as shallow ones (~9μs vs 108μs for depth 20).
- **Bulk reads are fast**: ReadFile outperforms streaming for whole-file reads.
- **KV is efficient**: Small value operations complete in single-digit microseconds.

*Benchmarks run on Apple M1 Ultra. Results vary by hardware and SQLite configuration.*

## Path Resolution Cache

The SDK includes an optional LRU cache for path-to-inode resolution, significantly improving performance for deep directory structures.

```go
afs, err := agentfs.Open(ctx, agentfs.AgentFSOptions{
    ID: "my-agent",
    Cache: agentfs.CacheOptions{
        Enabled:    true,
        MaxEntries: 10000,  // Max cached paths (default: 10000)
        TTL:        0,      // No expiration (recommended for single-process)
    },
})

// For multi-process scenarios, use TTL to handle external changes
afs, err := agentfs.Open(ctx, agentfs.AgentFSOptions{
    ID: "shared-agent",
    Cache: agentfs.CacheOptions{
        Enabled:    true,
        MaxEntries: 5000,
        TTL:        5 * time.Second,  // Re-validate after 5s
    },
})

// Check cache performance
if stats := afs.FS.CacheStats(); stats != nil {
    fmt.Printf("Cache hit rate: %.1f%%\n", stats.HitRate())
}

// Clear cache if external changes occurred
afs.FS.ClearCache()
```

The cache automatically invalidates entries when files/directories are deleted, renamed, or moved.

## Connection Pool

The SDK uses Go's `database/sql` connection pool. You can tune it for your workload:

```go
afs, err := agentfs.Open(ctx, agentfs.AgentFSOptions{
    ID: "my-agent",
    Pool: agentfs.PoolOptions{
        MaxOpenConns:    1,               // Serialize access (recommended for SQLite)
        MaxIdleConns:    1,               // Keep one connection ready
        ConnMaxLifetime: 0,               // Connections never expire
        ConnMaxIdleTime: 5 * time.Minute, // Close idle connections after 5m
    },
})
```

**Recommendations:**
- **Single-process**: `MaxOpenConns: 1` ensures serialized access, avoiding SQLite lock contention
- **Read-heavy**: Higher `MaxOpenConns` can improve read parallelism with WAL mode
- **Long-running**: Set `ConnMaxIdleTime` to periodically refresh connections

## Schema Compatibility

This SDK implements the AgentFS specification v0.4 and is compatible with databases created by:
- Python SDK (agentfs-py)
- TypeScript SDK (agentfs-ts)
- Rust SDK (agentfs-rs)

## License

See the main AgentFS repository for license information.
