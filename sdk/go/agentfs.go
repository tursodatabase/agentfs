// Package agentfs provides a SQLite-backed virtual filesystem, key-value store,
// and tool call tracking for AI agents.
package agentfs

import (
	"context"
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"

	_ "modernc.org/sqlite"
)

// AgentFS is the main entry point providing access to filesystem,
// key-value store, and tool call tracking.
type AgentFS struct {
	db     *sql.DB
	ownsDB bool // true if we opened the DB and should close it
	path   string

	// FS provides filesystem operations
	FS *Filesystem

	// KV provides key-value store operations
	KV *KVStore

	// Tools provides tool call tracking operations
	Tools *ToolCalls
}

// ErrSchemaVersionMismatch is returned when a database was created with an
// incompatible schema version.
type ErrSchemaVersionMismatch struct {
	Found    string
	Expected string
}

func (e *ErrSchemaVersionMismatch) Error() string {
	return fmt.Sprintf("schema version mismatch: found %q, expected %q", e.Found, e.Expected)
}

// validIDPattern matches valid agent IDs (alphanumeric, hyphens, underscores)
var validIDPattern = regexp.MustCompile(`^[a-zA-Z0-9_-]+$`)

// Open creates or opens an AgentFS database.
//
// If opts.Path is provided, it is used directly as the database path.
// If opts.ID is provided (without Path), the database is stored at ~/.agentfs/{id}.db
// At least one of Path or ID must be provided.
func Open(ctx context.Context, opts AgentFSOptions) (*AgentFS, error) {
	dbPath, err := resolveDBPath(opts)
	if err != nil {
		return nil, err
	}

	// Ensure parent directory exists
	dir := filepath.Dir(dbPath)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return nil, fmt.Errorf("failed to create directory %s: %w", dir, err)
	}

	// Open database
	db, err := sql.Open("sqlite", dbPath)
	if err != nil {
		return nil, fmt.Errorf("failed to open database: %w", err)
	}

	// Configure connection pool
	if opts.Pool.MaxOpenConns > 0 {
		db.SetMaxOpenConns(opts.Pool.MaxOpenConns)
	}
	if opts.Pool.MaxIdleConns > 0 {
		db.SetMaxIdleConns(opts.Pool.MaxIdleConns)
	}
	if opts.Pool.ConnMaxLifetime > 0 {
		db.SetConnMaxLifetime(opts.Pool.ConnMaxLifetime)
	}
	if opts.Pool.ConnMaxIdleTime > 0 {
		db.SetConnMaxIdleTime(opts.Pool.ConnMaxIdleTime)
	}

	// Enable WAL mode for better concurrency
	if _, err := db.ExecContext(ctx, "PRAGMA journal_mode=WAL"); err != nil {
		db.Close()
		return nil, fmt.Errorf("failed to enable WAL mode: %w", err)
	}

	// Enable foreign keys
	if _, err := db.ExecContext(ctx, "PRAGMA foreign_keys=ON"); err != nil {
		db.Close()
		return nil, fmt.Errorf("failed to enable foreign keys: %w", err)
	}

	afs, err := initAgentFS(ctx, db, dbPath, true, opts)
	if err != nil {
		db.Close()
		return nil, err
	}

	return afs, nil
}

// OpenWith creates an AgentFS using an existing *sql.DB connection.
//
// The caller retains ownership of the database connection. Calling Close()
// on the returned AgentFS will not close the underlying database.
//
// OpenWithOptions can be used to configure cache and chunk size. Pool options
// are ignored since the connection is externally managed.
func OpenWith(ctx context.Context, db *sql.DB, opts ...OpenWithOption) (*AgentFS, error) {
	o := openWithOptions{}
	for _, opt := range opts {
		opt(&o)
	}

	afsOpts := AgentFSOptions{
		ChunkSize: o.chunkSize,
	}

	afs, err := initAgentFS(ctx, db, "", false, afsOpts)
	if err != nil {
		return nil, err
	}

	return afs, nil
}

// OpenWithOption configures optional settings for OpenWith.
type OpenWithOption func(*openWithOptions)

type openWithOptions struct {
	chunkSize int
}

// WithChunkSize sets the chunk size for file data storage.
func WithChunkSize(size int) OpenWithOption {
	return func(o *openWithOptions) {
		o.chunkSize = size
	}
}

// initAgentFS contains the shared initialization logic for Open and OpenWith.
func initAgentFS(ctx context.Context, db *sql.DB, dbPath string, ownsDB bool, opts AgentFSOptions) (*AgentFS, error) {
	// Initialize schema
	if err := initSchema(ctx, db); err != nil {
		return nil, fmt.Errorf("failed to initialize schema: %w", err)
	}

	// Apply nanosecond timestamp migrations (backwards-compatible)
	// These add columns if they don't exist; errors are ignored for existing columns
	for _, stmt := range nsecMigrations() {
		db.ExecContext(ctx, stmt) // Ignore errors (column may already exist)
	}

	// Initialize and validate schema version
	if _, err := db.ExecContext(ctx, initSchemaVersion, schemaVersion); err != nil {
		return nil, fmt.Errorf("failed to initialize schema_version: %w", err)
	}

	var foundVersion string
	if err := db.QueryRowContext(ctx, getSchemaVersion).Scan(&foundVersion); err != nil {
		return nil, fmt.Errorf("failed to read schema_version: %w", err)
	}
	if foundVersion != schemaVersion {
		return nil, &ErrSchemaVersionMismatch{Found: foundVersion, Expected: schemaVersion}
	}

	// Determine chunk size
	chunkSize := opts.ChunkSize
	if chunkSize <= 0 {
		chunkSize = DefaultChunkSize
	}

	// Initialize filesystem config (chunk_size)
	if _, err := db.ExecContext(ctx, initFsConfig, strconv.Itoa(chunkSize)); err != nil {
		return nil, fmt.Errorf("failed to initialize fs_config: %w", err)
	}

	// Initialize root inode
	if _, err := db.ExecContext(ctx, initRootInode, DefaultDirMode); err != nil {
		return nil, fmt.Errorf("failed to initialize root inode: %w", err)
	}

	// Read actual chunk size from database (may differ if database already existed)
	var chunkSizeStr string
	if err := db.QueryRowContext(ctx, getChunkSize).Scan(&chunkSizeStr); err != nil {
		return nil, fmt.Errorf("failed to read chunk_size: %w", err)
	}
	actualChunkSize, err := strconv.Atoi(chunkSizeStr)
	if err != nil {
		return nil, fmt.Errorf("invalid chunk_size value: %w", err)
	}

	afs := &AgentFS{
		db:     db,
		ownsDB: ownsDB,
		path:   dbPath,
	}

	// Initialize subsystems
	afs.FS = &Filesystem{
		db:        db,
		chunkSize: actualChunkSize,
	}
	afs.KV = &KVStore{db: db}
	afs.Tools = &ToolCalls{db: db}

	return afs, nil
}

// Close closes the AgentFS instance.
// If the database was opened by Open, the connection is closed.
// If the database was provided via OpenWith, the connection is not closed.
func (a *AgentFS) Close() error {
	if a.ownsDB {
		return a.db.Close()
	}
	return nil
}

// Path returns the path to the underlying database file.
// Returns an empty string if the AgentFS was created via OpenWith.
func (a *AgentFS) Path() string {
	return a.path
}

// DB returns the underlying *sql.DB connection.
func (a *AgentFS) DB() *sql.DB {
	return a.db
}

// resolveDBPath determines the database file path from options
func resolveDBPath(opts AgentFSOptions) (string, error) {
	if opts.Path != "" {
		return opts.Path, nil
	}

	if opts.ID == "" {
		return "", fmt.Errorf("either Path or ID must be provided")
	}

	if !validIDPattern.MatchString(opts.ID) {
		return "", fmt.Errorf("invalid agent ID: must match pattern %s", validIDPattern.String())
	}

	// Default to ~/.agentfs/{id}.db
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("failed to get home directory: %w", err)
	}

	return filepath.Join(home, ".agentfs", opts.ID+".db"), nil
}

// initSchema creates all tables and indexes if they don't exist
func initSchema(ctx context.Context, db *sql.DB) error {
	for _, stmt := range allSchemaStatements() {
		if _, err := db.ExecContext(ctx, stmt); err != nil {
			return fmt.Errorf("schema creation failed: %w", err)
		}
	}
	return nil
}
