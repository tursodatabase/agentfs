# AgentFS Go SDK

A Go SDK for AgentFS - a SQLite-backed virtual filesystem, key-value store, and tool call tracking system for AI agents.

## Installation

```bash
go get agentfs/sdk/go
```

## Quick Start

```go
package main

import (
    "context"
    "fmt"
    "log"

    agentfs "agentfs/sdk/go"
)

func main() {
    ctx := context.Background()

    // Open or create an AgentFS database
    afs, err := agentfs.Open(ctx, agentfs.AgentFSOptions{
        ID: "my-agent",  // Creates ~/.agentfs/my-agent.db
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
    ID   string // Agent ID (creates ~/.agentfs/{id}.db)
    Path string // Explicit database path (takes precedence)
    ChunkSize int // Chunk size for file data (default: 4096)
}
```

### Filesystem

| Method | Description |
|--------|-------------|
| `Stat(path)` | Get file/directory metadata |
| `Readdir(path)` | List directory entries |
| `ReaddirPlus(path)` | List entries with stats |
| `Mkdir(path, mode)` | Create directory |
| `MkdirAll(path, mode)` | Create directory and parents |
| `ReadFile(path)` | Read entire file |
| `WriteFile(path, data, mode)` | Write file (creates parents) |
| `Unlink(path)` | Delete file |
| `Rmdir(path)` | Delete empty directory |
| `Rename(old, new)` | Move/rename file or directory |
| `Link(existing, new)` | Create hard link |
| `Symlink(target, link)` | Create symbolic link |
| `Readlink(path)` | Read symlink target |
| `Chmod(path, mode)` | Change permissions |
| `Utimes(path, atime, mtime)` | Update timestamps |
| `Open(path, flags)` | Open file handle |
| `Create(path, mode)` | Create new file handle |

### File Handle

```go
type File struct { ... }

func (f *File) Pread(ctx, buf, offset) (int, error)  // Positioned read
func (f *File) Pwrite(ctx, data, offset) (int, error) // Positioned write
func (f *File) Truncate(ctx, size) error             // Set file size
func (f *File) Fsync(ctx) error                      // Flush to storage
func (f *File) Stat(ctx) (*Stats, error)             // Get metadata
func (f *File) Close() error                         // Close handle
```

### Key-Value Store

| Method | Description |
|--------|-------------|
| `Set(key, value)` | Store value (JSON-serialized) |
| `Get(key, dest)` | Retrieve and unmarshal value |
| `GetRaw(key)` | Get raw JSON value |
| `Delete(key)` | Remove key |
| `Has(key)` | Check if key exists |
| `Keys(prefix)` | List keys (optionally by prefix) |
| `List(prefix)` | List keys with metadata |
| `Clear(prefix)` | Delete keys (optionally by prefix) |

### Tool Calls

| Method | Description |
|--------|-------------|
| `Start(name, params)` | Begin tracking a call |
| `PendingCall.Success(result)` | Mark as successful |
| `PendingCall.Error(err)` | Mark as failed |
| `Record(...)` | Insert complete record |
| `Get(id)` | Get call by ID |
| `GetByName(name, limit)` | Get calls by name |
| `GetRecent(since, limit)` | Get recent calls |
| `GetStats()` | Get aggregated statistics |

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

| Interface | Status |
|-----------|--------|
| `fs.FS` | Supported |
| `fs.StatFS` | Supported |
| `fs.ReadFileFS` | Supported |
| `fs.ReadDirFS` | Supported |
| `fs.SubFS` | Supported |
| `fs.ReadDirFile` | Supported (for directories) |

**Note:** Use `fs.Glob(iofs, pattern)` for glob matching - it uses the `ReadDir` implementation.

## Schema Compatibility

This SDK implements the AgentFS specification v0.4 and is compatible with databases created by:
- Python SDK (agentfs-py)
- TypeScript SDK (agentfs-ts)
- Rust SDK (agentfs-rs)

## License

See the main AgentFS repository for license information.
