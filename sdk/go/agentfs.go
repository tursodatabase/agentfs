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

	"github.com/tursodatabase/agentfs/sdk/go/internal/cache"

	_ "modernc.org/sqlite"
)

// AgentFS is the main entry point providing access to filesystem,
// key-value store, and tool call tracking.
type AgentFS struct {
	db   *sql.DB
	path string

	// FS provides filesystem operations
	FS *Filesystem

	// KV provides key-value store operations
	KV *KVStore

	// Tools provides tool call tracking operations
	Tools *ToolCalls
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

	// Initialize schema
	if err := initSchema(ctx, db); err != nil {
		db.Close()
		return nil, fmt.Errorf("failed to initialize schema: %w", err)
	}

	// Apply nanosecond timestamp migrations (backwards-compatible)
	// These add columns if they don't exist; errors are ignored for existing columns
	for _, stmt := range nsecMigrations() {
		db.ExecContext(ctx, stmt) // Ignore errors (column may already exist)
	}

	// Determine chunk size
	chunkSize := opts.ChunkSize
	if chunkSize <= 0 {
		chunkSize = DefaultChunkSize
	}

	// Initialize filesystem config (chunk_size)
	if _, err := db.ExecContext(ctx, initFsConfig, strconv.Itoa(chunkSize)); err != nil {
		db.Close()
		return nil, fmt.Errorf("failed to initialize fs_config: %w", err)
	}

	// Initialize root inode
	if _, err := db.ExecContext(ctx, initRootInode, DefaultDirMode); err != nil {
		db.Close()
		return nil, fmt.Errorf("failed to initialize root inode: %w", err)
	}

	// Read actual chunk size from database (may differ if database already existed)
	var chunkSizeStr string
	if err := db.QueryRowContext(ctx, getChunkSize).Scan(&chunkSizeStr); err != nil {
		db.Close()
		return nil, fmt.Errorf("failed to read chunk_size: %w", err)
	}
	actualChunkSize, err := strconv.Atoi(chunkSizeStr)
	if err != nil {
		db.Close()
		return nil, fmt.Errorf("invalid chunk_size value: %w", err)
	}

	afs := &AgentFS{
		db:   db,
		path: dbPath,
	}

	// Initialize cache if enabled
	var pathCache cache.PathCache
	if opts.Cache.Enabled {
		maxEntries := opts.Cache.MaxEntries
		if maxEntries <= 0 {
			maxEntries = 10000 // Default
		}
		var err error
		pathCache, err = cache.NewLRU(maxEntries, opts.Cache.TTL)
		if err != nil {
			db.Close()
			return nil, fmt.Errorf("failed to initialize cache: %w", err)
		}
	}

	// Initialize subsystems
	afs.FS = &Filesystem{
		db:        db,
		chunkSize: actualChunkSize,
		cache:     pathCache,
	}
	afs.KV = &KVStore{db: db}
	afs.Tools = &ToolCalls{db: db}

	return afs, nil
}

// Close closes the database connection.
func (a *AgentFS) Close() error {
	return a.db.Close()
}

// Path returns the path to the underlying database file.
func (a *AgentFS) Path() string {
	return a.path
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
