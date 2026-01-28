package agentfs

import (
	"database/sql"

	_ "turso.tech/database/tursogo"
)

/**
 * Configuration options for opening an AgentFS instance
 */
type AgentFSOptions struct {
	/**
	 * Unique identifier for the agent.
	 * - If provided without `path`: Creates storage at `.agentfs/{id}.db`
	 * - If provided with `path`: Uses the specified path
	 */
	Id string
	/**
	 * Explicit path to the database file.
	 * - If provided: Uses the specified path directly
	 * - Can be combined with `id`
	 */
	Path string
}

type AgentFSCore struct {
	db    *sql.DB
	Kv    KvStore
	Fs    FileSystem
	Tools ToolCalls
}

func NewAgentFSCore(db *sql.DB, kv KvStore, fs FileSystem, tools ToolCalls) *AgentFSCore {
	return &AgentFSCore{
		db:    db,
		Kv:    kv,
		Fs:    fs,
		Tools: tools,
	}
}

/**
 * Get the underlying Database instance
 */
func (core *AgentFSCore) GetDatabase() *sql.DB {
	return core.db
}

/**
 * Close the database connection
 */
func (core *AgentFS) Close() error {
	return core.db.Close()
}
