package agentfs

import (
	"context"
	"encoding/json"
)

// FileSystem defines the interface for filesystem operations.
// This interface is optional - the SDK returns concrete *Filesystem types,
// but users can program against this interface for testability and mocking.
type FileSystem interface {
	// Stat returns file/directory metadata for the given path.
	// If the path refers to a symlink, Stat follows the symlink.
	Stat(ctx context.Context, path string) (*Stats, error)

	// Lstat returns file/directory metadata without following the final symlink.
	Lstat(ctx context.Context, path string) (*Stats, error)

	// Readdir returns the names of entries in a directory.
	Readdir(ctx context.Context, path string) ([]string, error)

	// ReaddirPlus returns directory entries with their stats.
	ReaddirPlus(ctx context.Context, path string) ([]DirEntry, error)

	// Mkdir creates a directory.
	Mkdir(ctx context.Context, path string, mode int64) error

	// MkdirAll creates a directory and all parent directories as needed.
	MkdirAll(ctx context.Context, path string, mode int64) error

	// ReadFile reads the entire contents of a file.
	ReadFile(ctx context.Context, path string) ([]byte, error)

	// WriteFile writes data to a file, creating it if it doesn't exist.
	WriteFile(ctx context.Context, path string, data []byte, mode int64) error

	// Unlink removes a file.
	Unlink(ctx context.Context, path string) error

	// Rmdir removes an empty directory.
	Rmdir(ctx context.Context, path string) error

	// Rename moves or renames a file or directory.
	Rename(ctx context.Context, oldPath, newPath string) error

	// Link creates a hard link.
	Link(ctx context.Context, existingPath, newPath string) error

	// Symlink creates a symbolic link.
	Symlink(ctx context.Context, target, linkPath string) error

	// Readlink returns the target of a symbolic link.
	Readlink(ctx context.Context, path string) (string, error)

	// Chmod changes file permissions.
	Chmod(ctx context.Context, path string, mode int64) error

	// Utimes updates file timestamps.
	Utimes(ctx context.Context, path string, atime, mtime int64) error

	// Utimens updates file timestamps with selective control.
	// Each timestamp can be TimeSet (specific value), TimeNow (current time),
	// or TimeOmit (leave unchanged).
	Utimens(ctx context.Context, path string, atime, mtime TimeChange) error

	// Open opens a file and returns a handle for read/write operations.
	Open(ctx context.Context, path string, flags int) (*File, error)

	// Create creates a new file and returns its stats and a handle.
	Create(ctx context.Context, path string, mode int64) (*Stats, *File, error)

	// ChunkSize returns the configured chunk size for file data.
	ChunkSize() int
}

// KVStoreInterface defines the interface for key-value store operations.
// This interface is optional - the SDK returns concrete *KVStore types,
// but users can program against this interface for testability and mocking.
type KVStoreInterface interface {
	// Set stores a value (JSON-serialized) for the given key.
	Set(ctx context.Context, key string, value any) error

	// Get retrieves a value and unmarshals it into dest.
	Get(ctx context.Context, key string, dest any) error

	// GetRaw retrieves the raw JSON value for a key.
	GetRaw(ctx context.Context, key string) (json.RawMessage, error)

	// Delete removes a key.
	Delete(ctx context.Context, key string) error

	// Has checks if a key exists.
	Has(ctx context.Context, key string) (bool, error)

	// Keys returns all keys, optionally filtered by prefix.
	Keys(ctx context.Context, prefix string) ([]string, error)

	// List returns all key entries with metadata, optionally filtered by prefix.
	List(ctx context.Context, prefix string) ([]KVEntry, error)

	// Clear removes all keys, optionally filtered by prefix.
	Clear(ctx context.Context, prefix string) error
}

// ToolCallsInterface defines the interface for tool call tracking operations.
// This interface is optional - the SDK returns concrete *ToolCalls types,
// but users can program against this interface for testability and mocking.
type ToolCallsInterface interface {
	// Start begins tracking a tool call, returning a PendingCall for later completion.
	Start(ctx context.Context, name string, parameters any) (*PendingCall, error)

	// Record inserts a complete tool call record directly.
	// The errMsg parameter is the error message string (nil if successful).
	Record(ctx context.Context, name string, parameters any, result any, errMsg *string, startedAt, completedAt int64) (*ToolCall, error)

	// Get retrieves a tool call by ID.
	Get(ctx context.Context, id int64) (*ToolCall, error)

	// GetByName retrieves tool calls by name.
	GetByName(ctx context.Context, name string, limit int) ([]ToolCall, error)

	// GetRecent retrieves recent tool calls since the given timestamp.
	GetRecent(ctx context.Context, since int64, limit int) ([]ToolCall, error)

	// GetStats returns aggregated statistics for all tool calls.
	GetStats(ctx context.Context) ([]ToolCallStats, error)
}

// Compile-time interface satisfaction checks.
// These ensure the concrete types implement their respective interfaces.
var (
	_ FileSystem         = (*Filesystem)(nil)
	_ KVStoreInterface   = (*KVStore)(nil)
	_ ToolCallsInterface = (*ToolCalls)(nil)
)
