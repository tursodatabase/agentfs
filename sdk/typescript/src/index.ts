import { Database } from '@tursodatabase/database';
import { existsSync, mkdirSync } from 'fs';
import { homedir } from 'os';
import { join } from 'path';
import { KvStore } from './kvstore';
import { Filesystem } from './filesystem';
import { ToolCalls } from './toolcalls';

/**
 * Global directory containing agentfs databases (~/.agentfs/fs)
 * This is the default location for new agent databases.
 */
export function agentfsDir(): string {
  return join(homedir(), '.agentfs', 'fs');
}

/**
 * Local directory containing agentfs databases (.agentfs in current directory)
 * This is checked first when resolving agent IDs for backward compatibility.
 */
export function localAgentfsDir(): string {
  return '.agentfs';
}

/**
 * Configuration options for opening an AgentFS instance
 */
export interface AgentFSOptions {
  /**
   * Unique identifier for the agent.
   * - If provided without `path`: Creates storage at `~/.agentfs/fs/{id}.db`
   * - If provided with `path`: Uses the specified path
   */
  id?: string;
  /**
   * Explicit path to the database file.
   * - If provided: Uses the specified path directly
   * - Can be combined with `id`
   */
  path?: string;
}

export class AgentFS {
  private db: Database;

  public readonly kv: KvStore;
  public readonly fs: Filesystem;
  public readonly tools: ToolCalls;

  /**
   * Private constructor - use AgentFS.open() instead
   */
  private constructor(db: Database, kv: KvStore, fs: Filesystem, tools: ToolCalls) {
    this.db = db;
    this.kv = kv;
    this.fs = fs;
    this.tools = tools;
  }

  /**
   * Resolve an id-or-path string to a database path
   *
   * - Existing file path -> uses that path directly
   * - Otherwise -> treats as agent ID, looks for database in this order:
   *   1. `.agentfs/{id}.db` (local directory)
   *   2. `~/.agentfs/fs/{id}.db` (global directory)
   *
   * @param idOrPath Agent ID or path to database file
   * @returns Resolved database path
   * @throws Error if the agent ID is invalid or the database doesn't exist
   */
  static resolve(idOrPath: string): string {
    // Check if it's an existing file path
    if (existsSync(idOrPath)) {
      return idOrPath;
    }

    // Treat as an agent ID - validate for safety
    if (!/^[a-zA-Z0-9_-]+$/.test(idOrPath)) {
      throw new Error(
        `Invalid agent ID '${idOrPath}'. Agent IDs must contain only alphanumeric characters, hyphens, and underscores.`
      );
    }

    // Check local directory first (.agentfs in current directory)
    const localDbPath = join(localAgentfsDir(), `${idOrPath}.db`);
    if (existsSync(localDbPath)) {
      return localDbPath;
    }

    // Then check global directory (~/.agentfs/fs)
    const globalDbPath = join(agentfsDir(), `${idOrPath}.db`);
    if (existsSync(globalDbPath)) {
      return globalDbPath;
    }

    throw new Error(
      `Agent '${idOrPath}' not found in local (.agentfs) or global (~/.agentfs/fs) directories`
    );
  }

  /**
   * Open an agent filesystem
   * @param options Configuration options (id and/or path required)
   * @returns Fully initialized AgentFS instance
   * @example
   * ```typescript
   * // Using id (creates ~/.agentfs/fs/my-agent.db)
   * const agent = await AgentFS.open({ id: 'my-agent' });
   *
   * // Using id with custom path
   * const agent = await AgentFS.open({ id: 'my-agent', path: './data/mydb.db' });
   *
   * // Using path only
   * const agent = await AgentFS.open({ path: './data/mydb.db' });
   * ```
   */
  static async open(options: AgentFSOptions): Promise<AgentFS> {
    const { id, path } = options;

    // Require at least id or path
    if (!id && !path) {
      throw new Error("AgentFS.open() requires at least 'id' or 'path'.");
    }

    // Validate agent ID if provided
    if (id && !/^[a-zA-Z0-9_-]+$/.test(id)) {
      throw new Error(
        'Agent ID must contain only alphanumeric characters, hyphens, and underscores'
      );
    }

    // Determine database path: explicit path takes precedence, otherwise use id-based path
    let dbPath: string;
    if (path) {
      dbPath = path;
    } else {
      // id is guaranteed to be defined here (we checked !id && !path above)
      const dir = agentfsDir();
      if (!existsSync(dir)) {
        mkdirSync(dir, { recursive: true });
      }
      dbPath = join(dir, `${id}.db`);
    }

    const db = new Database(dbPath);

    // Connect to the database to ensure it's created
    await db.connect();

    return await AgentFS.openWith(db);
  }

  static async openWith(db: Database): Promise<AgentFS> {
    const [kv, fs, tools] = await Promise.all([
      KvStore.fromDatabase(db),
      Filesystem.fromDatabase(db),
      ToolCalls.fromDatabase(db),
    ]);
    return new AgentFS(db, kv, fs, tools);
  }

  /**
   * Get the underlying Database instance
   */
  getDatabase(): Database {
    return this.db;
  }

  /**
   * Close the database connection
   */
  async close(): Promise<void> {
    await this.db.close();
  }
}

export { KvStore } from './kvstore';
export { Filesystem } from './filesystem';
export type { Stats } from './filesystem';
export { ToolCalls } from './toolcalls';
export type { ToolCall, ToolCallStats } from './toolcalls';
