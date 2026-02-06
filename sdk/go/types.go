package agentfs

import (
	"encoding/json"
	"time"
)

// AgentFSOptions configures how AgentFS opens or creates a database
type AgentFSOptions struct {
	// ID is the agent identifier. If provided without Path, the database
	// will be stored at ~/.agentfs/{id}.db
	// Must match pattern: ^[a-zA-Z0-9_-]+$
	ID string

	// Path is an explicit database file path. Takes precedence over ID.
	Path string

	// ChunkSize is the size of data chunks in bytes (default: 4096)
	// Only used when creating a new database; ignored for existing databases.
	ChunkSize int

	// Pool configures the database connection pool.
	Pool PoolOptions
}

// PoolOptions configures the SQLite connection pool.
// These settings control how database/sql manages connections.
type PoolOptions struct {
	// MaxOpenConns sets the maximum number of open connections to the database.
	// Default: 0 (unlimited). For SQLite, a value of 1 ensures serialized access.
	MaxOpenConns int

	// MaxIdleConns sets the maximum number of connections in the idle pool.
	// Default: 2 (database/sql default). Set to MaxOpenConns for best performance.
	MaxIdleConns int

	// ConnMaxLifetime sets the maximum amount of time a connection may be reused.
	// Default: 0 (connections are reused forever).
	ConnMaxLifetime time.Duration

	// ConnMaxIdleTime sets the maximum amount of time a connection may be idle.
	// Default: 0 (connections are never closed due to idle time).
	ConnMaxIdleTime time.Duration
}

// Stats represents file/directory metadata (matches POSIX stat)
type Stats struct {
	Ino       int64 `json:"ino"`        // Inode number
	Mode      int64 `json:"mode"`       // File type and permissions
	Nlink     int64 `json:"nlink"`      // Number of hard links
	UID       int64 `json:"uid"`        // Owner user ID
	GID       int64 `json:"gid"`        // Owner group ID
	Size      int64 `json:"size"`       // File size in bytes
	Atime     int64 `json:"atime"`      // Last access time (Unix timestamp, seconds)
	Mtime     int64 `json:"mtime"`      // Last modification time (Unix timestamp, seconds)
	Ctime     int64 `json:"ctime"`      // Creation/change time (Unix timestamp, seconds)
	Rdev      int64 `json:"rdev"`       // Device number (for special files)
	AtimeNsec int64 `json:"atime_nsec"` // Nanosecond component of atime (0-999999999)
	MtimeNsec int64 `json:"mtime_nsec"` // Nanosecond component of mtime (0-999999999)
	CtimeNsec int64 `json:"ctime_nsec"` // Nanosecond component of ctime (0-999999999)
}

// IsDir returns true if this is a directory
func (s *Stats) IsDir() bool {
	return (s.Mode & S_IFMT) == S_IFDIR
}

// IsRegularFile returns true if this is a regular file
func (s *Stats) IsRegularFile() bool {
	return (s.Mode & S_IFMT) == S_IFREG
}

// IsSymlink returns true if this is a symbolic link
func (s *Stats) IsSymlink() bool {
	return (s.Mode & S_IFMT) == S_IFLNK
}

// IsFIFO returns true if this is a FIFO/named pipe
func (s *Stats) IsFIFO() bool {
	return (s.Mode & S_IFMT) == S_IFIFO
}

// IsCharDevice returns true if this is a character device
func (s *Stats) IsCharDevice() bool {
	return (s.Mode & S_IFMT) == S_IFCHR
}

// IsBlockDevice returns true if this is a block device
func (s *Stats) IsBlockDevice() bool {
	return (s.Mode & S_IFMT) == S_IFBLK
}

// IsSocket returns true if this is a socket
func (s *Stats) IsSocket() bool {
	return (s.Mode & S_IFMT) == S_IFSOCK
}

// FileType returns the file type portion of the mode
func (s *Stats) FileType() int64 {
	return s.Mode & S_IFMT
}

// Permissions returns the permission bits of the mode
func (s *Stats) Permissions() int64 {
	return s.Mode & 0o777
}

// AtimeTime returns atime as a time.Time with nanosecond precision
func (s *Stats) AtimeTime() time.Time {
	return time.Unix(s.Atime, s.AtimeNsec)
}

// MtimeTime returns mtime as a time.Time with nanosecond precision
func (s *Stats) MtimeTime() time.Time {
	return time.Unix(s.Mtime, s.MtimeNsec)
}

// CtimeTime returns ctime as a time.Time with nanosecond precision
func (s *Stats) CtimeTime() time.Time {
	return time.Unix(s.Ctime, s.CtimeNsec)
}

// TimeChange represents a timestamp change request for Utimens.
// Use the constructor functions TimeOmit(), TimeNow(), and TimeSet() to create values.
type TimeChange struct {
	kind int // 0=Omit, 1=Now, 2=Set
	sec  int64
	nsec int64
}

const (
	timeOmit = 0
	timeNow  = 1
	timeSet  = 2
)

// TimeOmit returns a TimeChange that leaves the timestamp unchanged.
func TimeOmit() TimeChange {
	return TimeChange{kind: timeOmit}
}

// TimeNow returns a TimeChange that sets the timestamp to the current time.
func TimeNow() TimeChange {
	return TimeChange{kind: timeNow}
}

// TimeSet returns a TimeChange that sets the timestamp to the given value.
func TimeSet(sec, nsec int64) TimeChange {
	return TimeChange{kind: timeSet, sec: sec, nsec: nsec}
}

// IsOmit returns true if this TimeChange leaves the timestamp unchanged.
func (tc TimeChange) IsOmit() bool { return tc.kind == timeOmit }

// IsNow returns true if this TimeChange sets the timestamp to the current time.
func (tc TimeChange) IsNow() bool { return tc.kind == timeNow }

// IsSet returns true if this TimeChange sets the timestamp to a specific value.
func (tc TimeChange) IsSet() bool { return tc.kind == timeSet }

// FilesystemStats represents aggregate filesystem statistics returned by Statfs.
type FilesystemStats struct {
	Inodes    int64 `json:"inodes"`     // Total number of inodes (files, directories, symlinks, etc.)
	BytesUsed int64 `json:"bytes_used"` // Total bytes used by file contents
}

// DirEntry represents a directory entry returned by ReaddirPlus
type DirEntry struct {
	Name  string `json:"name"`
	Stats *Stats `json:"stats"`
}

// ToolCall represents a recorded tool invocation
type ToolCall struct {
	ID          int64           `json:"id"`
	Name        string          `json:"name"`
	Parameters  json.RawMessage `json:"parameters,omitempty"`
	Result      json.RawMessage `json:"result,omitempty"`
	Error       *string         `json:"error,omitempty"`
	StartedAt   int64           `json:"started_at"`
	CompletedAt int64           `json:"completed_at"`
	DurationMs  int64           `json:"duration_ms"`
}

// ToolCallStats represents aggregated statistics for tool calls
type ToolCallStats struct {
	Name          string  `json:"name"`
	TotalCalls    int64   `json:"total_calls"`
	Successful    int64   `json:"successful"`
	Failed        int64   `json:"failed"`
	AvgDurationMs float64 `json:"avg_duration_ms"`
}

// KVEntry represents a key-value pair with metadata
type KVEntry struct {
	Key       string `json:"key"`
	CreatedAt int64  `json:"created_at"`
	UpdatedAt int64  `json:"updated_at"`
}
