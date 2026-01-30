package agentfs

// SQL schema definitions for AgentFS

const schemaVersion = "0.4"

// Table creation statements
const (
	createFsConfigTable = `
		CREATE TABLE IF NOT EXISTS fs_config (
			key TEXT PRIMARY KEY,
			value TEXT NOT NULL
		)`

	createFsInodeTable = `
		CREATE TABLE IF NOT EXISTS fs_inode (
			ino INTEGER PRIMARY KEY AUTOINCREMENT,
			mode INTEGER NOT NULL,
			nlink INTEGER NOT NULL DEFAULT 0,
			uid INTEGER NOT NULL DEFAULT 0,
			gid INTEGER NOT NULL DEFAULT 0,
			size INTEGER NOT NULL DEFAULT 0,
			atime INTEGER NOT NULL,
			mtime INTEGER NOT NULL,
			ctime INTEGER NOT NULL,
			rdev INTEGER NOT NULL DEFAULT 0
		)`

	createFsDentryTable = `
		CREATE TABLE IF NOT EXISTS fs_dentry (
			id INTEGER PRIMARY KEY AUTOINCREMENT,
			name TEXT NOT NULL,
			parent_ino INTEGER NOT NULL,
			ino INTEGER NOT NULL,
			UNIQUE(parent_ino, name)
		)`

	createFsDentryIndex = `
		CREATE INDEX IF NOT EXISTS idx_fs_dentry_parent ON fs_dentry(parent_ino, name)`

	createFsDataTable = `
		CREATE TABLE IF NOT EXISTS fs_data (
			ino INTEGER NOT NULL,
			chunk_index INTEGER NOT NULL,
			data BLOB NOT NULL,
			PRIMARY KEY (ino, chunk_index)
		)`

	createFsSymlinkTable = `
		CREATE TABLE IF NOT EXISTS fs_symlink (
			ino INTEGER PRIMARY KEY,
			target TEXT NOT NULL
		)`

	createKvStoreTable = `
		CREATE TABLE IF NOT EXISTS kv_store (
			key TEXT PRIMARY KEY,
			value TEXT NOT NULL,
			created_at INTEGER DEFAULT (unixepoch()),
			updated_at INTEGER DEFAULT (unixepoch())
		)`

	createKvStoreIndex = `
		CREATE INDEX IF NOT EXISTS idx_kv_store_created_at ON kv_store(created_at)`

	createToolCallsTable = `
		CREATE TABLE IF NOT EXISTS tool_calls (
			id INTEGER PRIMARY KEY AUTOINCREMENT,
			name TEXT NOT NULL,
			parameters TEXT,
			result TEXT,
			error TEXT,
			started_at INTEGER NOT NULL,
			completed_at INTEGER NOT NULL,
			duration_ms INTEGER NOT NULL
		)`

	createToolCallsNameIndex = `
		CREATE INDEX IF NOT EXISTS idx_tool_calls_name ON tool_calls(name)`

	createToolCallsStartedAtIndex = `
		CREATE INDEX IF NOT EXISTS idx_tool_calls_started_at ON tool_calls(started_at)`

	// Overlay filesystem tables (optional)
	createFsWhiteoutTable = `
		CREATE TABLE IF NOT EXISTS fs_whiteout (
			path TEXT PRIMARY KEY,
			parent_path TEXT NOT NULL,
			created_at INTEGER NOT NULL
		)`

	createFsWhiteoutIndex = `
		CREATE INDEX IF NOT EXISTS idx_fs_whiteout_parent ON fs_whiteout(parent_path)`

	createFsOriginTable = `
		CREATE TABLE IF NOT EXISTS fs_origin (
			delta_ino INTEGER PRIMARY KEY,
			base_ino INTEGER NOT NULL
		)`
)

// allSchemaStatements returns all schema creation statements in order
func allSchemaStatements() []string {
	return []string{
		createFsConfigTable,
		createFsInodeTable,
		createFsDentryTable,
		createFsDentryIndex,
		createFsDataTable,
		createFsSymlinkTable,
		createKvStoreTable,
		createKvStoreIndex,
		createToolCallsTable,
		createToolCallsNameIndex,
		createToolCallsStartedAtIndex,
		createFsWhiteoutTable,
		createFsWhiteoutIndex,
		createFsOriginTable,
	}
}

// Nanosecond timestamp migrations (backwards-compatible)
// These use ALTER TABLE which will fail silently if columns already exist
const (
	migrateAddAtimeNsec = `ALTER TABLE fs_inode ADD COLUMN atime_nsec INTEGER NOT NULL DEFAULT 0`
	migrateAddMtimeNsec = `ALTER TABLE fs_inode ADD COLUMN mtime_nsec INTEGER NOT NULL DEFAULT 0`
	migrateAddCtimeNsec = `ALTER TABLE fs_inode ADD COLUMN ctime_nsec INTEGER NOT NULL DEFAULT 0`
)

// nsecMigrations returns the nanosecond column migration statements
func nsecMigrations() []string {
	return []string{
		migrateAddAtimeNsec,
		migrateAddMtimeNsec,
		migrateAddCtimeNsec,
	}
}

// Initialization queries
const (
	initFsConfig = `
		INSERT OR IGNORE INTO fs_config (key, value) VALUES ('chunk_size', ?)`

	initRootInode = `
		INSERT OR IGNORE INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
		VALUES (1, ?, 1, 0, 0, 0, unixepoch(), unixepoch(), unixepoch())`

	getChunkSize = `
		SELECT value FROM fs_config WHERE key = 'chunk_size'`
)

// Filesystem queries
const (
	// Path resolution
	queryDentryByParentAndName = `
		SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?`

	// Inode operations
	queryInodeByIno = `
		SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
		       atime_nsec, mtime_nsec, ctime_nsec
		FROM fs_inode WHERE ino = ?`

	insertInode = `
		INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
		                      atime_nsec, mtime_nsec, ctime_nsec)
		VALUES (?, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		RETURNING ino`

	updateInodeSize = `
		UPDATE fs_inode SET size = ?, mtime = ?, mtime_nsec = ? WHERE ino = ?`

	updateInodeTimes = `
		UPDATE fs_inode SET atime = ?, mtime = ?, ctime = ?,
		                    atime_nsec = ?, mtime_nsec = ?, ctime_nsec = ?
		WHERE ino = ?`

	updateInodeAtime = `
		UPDATE fs_inode SET atime = ?, atime_nsec = ? WHERE ino = ?`

	updateInodeMode = `
		UPDATE fs_inode SET mode = ?, ctime = ?, ctime_nsec = ? WHERE ino = ?`

	incrementNlink = `
		UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?`

	decrementNlink = `
		UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?`

	deleteInode = `
		DELETE FROM fs_inode WHERE ino = ?`

	// Directory entry operations
	insertDentry = `
		INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)`

	deleteDentry = `
		DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?`

	queryDentriesByParent = `
		SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name ASC`

	queryDentriesPlusByParent = `
		SELECT d.name, i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime, i.rdev,
		       i.atime_nsec, i.mtime_nsec, i.ctime_nsec
		FROM fs_dentry d
		JOIN fs_inode i ON d.ino = i.ino
		WHERE d.parent_ino = ?
		ORDER BY d.name ASC`

	countDentriesByParent = `
		SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?`

	updateDentryParent = `
		UPDATE fs_dentry SET parent_ino = ?, name = ? WHERE parent_ino = ? AND name = ?`

	// File data operations
	insertChunk = `
		INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)`

	queryChunksByIno = `
		SELECT data FROM fs_data WHERE ino = ? ORDER BY chunk_index ASC`

	queryChunkRange = `
		SELECT chunk_index, data FROM fs_data
		WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ?
		ORDER BY chunk_index ASC`

	deleteChunksByIno = `
		DELETE FROM fs_data WHERE ino = ?`

	deleteChunksFromIndex = `
		DELETE FROM fs_data WHERE ino = ? AND chunk_index >= ?`

	// Symlink operations
	insertSymlink = `
		INSERT INTO fs_symlink (ino, target) VALUES (?, ?)`

	querySymlinkTarget = `
		SELECT target FROM fs_symlink WHERE ino = ?`

	deleteSymlink = `
		DELETE FROM fs_symlink WHERE ino = ?`
)

// Key-value store queries
const (
	kvSet = `
		INSERT INTO kv_store (key, value, updated_at)
		VALUES (?, ?, unixepoch())
		ON CONFLICT(key) DO UPDATE SET
			value = excluded.value,
			updated_at = unixepoch()`

	kvGet = `
		SELECT value FROM kv_store WHERE key = ?`

	kvDelete = `
		DELETE FROM kv_store WHERE key = ?`

	kvHas = `
		SELECT 1 FROM kv_store WHERE key = ? LIMIT 1`

	kvKeys = `
		SELECT key FROM kv_store ORDER BY key ASC`

	kvKeysWithPrefix = `
		SELECT key FROM kv_store WHERE key LIKE ? ESCAPE '\' ORDER BY key ASC`

	kvList = `
		SELECT key, created_at, updated_at FROM kv_store ORDER BY key ASC`

	kvListWithPrefix = `
		SELECT key, created_at, updated_at FROM kv_store WHERE key LIKE ? ESCAPE '\' ORDER BY key ASC`

	kvClear = `
		DELETE FROM kv_store`

	kvClearWithPrefix = `
		DELETE FROM kv_store WHERE key LIKE ? ESCAPE '\'`
)

// Tool calls queries
const (
	toolCallsInsert = `
		INSERT INTO tool_calls (name, parameters, result, error, started_at, completed_at, duration_ms)
		VALUES (?, ?, ?, ?, ?, ?, ?)
		RETURNING id`

	toolCallsGetByID = `
		SELECT id, name, parameters, result, error, started_at, completed_at, duration_ms
		FROM tool_calls WHERE id = ?`

	toolCallsGetByName = `
		SELECT id, name, parameters, result, error, started_at, completed_at, duration_ms
		FROM tool_calls WHERE name = ?
		ORDER BY started_at DESC
		LIMIT ?`

	toolCallsGetRecent = `
		SELECT id, name, parameters, result, error, started_at, completed_at, duration_ms
		FROM tool_calls WHERE started_at > ?
		ORDER BY started_at DESC
		LIMIT ?`

	toolCallsGetStats = `
		SELECT
			name,
			COUNT(*) as total_calls,
			SUM(CASE WHEN error IS NULL THEN 1 ELSE 0 END) as successful,
			SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END) as failed,
			AVG(duration_ms) as avg_duration_ms
		FROM tool_calls
		GROUP BY name
		ORDER BY total_calls DESC`
)
