import type { DatabasePromise } from '@tursodatabase/database-common';
import { KvStore } from './kvstore.js';
import { AgentFS as Filesystem } from './filesystem/index.js';
import { ToolCalls } from './toolcalls.js';

/**
 * Configuration options for opening an AgentFS instance
 */
export interface AgentFSOptions {
  /**
   * Unique identifier for the agent.
   * - If provided without `path`: Creates storage at `.agentfs/{id}.db`
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

export class AgentFSCore {
  private db: DatabasePromise;

  public readonly kv: KvStore;
  public readonly fs: Filesystem;
  public readonly tools: ToolCalls;

  /**
   * Private constructor - use AgentFS.open() instead
   */
  protected constructor(db: DatabasePromise, kv: KvStore, fs: Filesystem, tools: ToolCalls) {
    this.db = db;
    this.kv = kv;
    this.fs = fs;
    this.tools = tools;
  }

  /**
   * Get the underlying Database instance
   */
  getDatabase(): DatabasePromise {
    return this.db;
  }

  /**
   * Close the database connection
   */
  async close(): Promise<void> {
    await this.db.close();
  }
}
