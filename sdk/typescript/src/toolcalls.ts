import type { DatabasePromise } from '@tursodatabase/database-common';

export interface ToolCall {
  id: number;
  name: string;
  parameters?: any;
  result?: any;
  error?: string;
  status: 'pending' | 'success' | 'error';
  started_at: number;
  completed_at?: number;
  duration_ms?: number;
}

export interface ToolCallStats {
  name: string;
  total_calls: number;
  successful: number;
  failed: number;
  avg_duration_ms: number;
}

export class ToolCalls {
  private db: DatabasePromise;

  private constructor(db: DatabasePromise) {
    this.db = db;
  }

  /**
   * Create a ToolCalls from an existing database connection
   */
  static async fromDatabase(db: DatabasePromise): Promise<ToolCalls> {
    const tools = new ToolCalls(db);
    await tools.initialize();
    return tools;
  }

  private async initialize(): Promise<void> {
    // Create the tool_calls table
    await this.db.exec(`
      CREATE TABLE IF NOT EXISTS tool_calls (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        parameters TEXT,
        result TEXT,
        error TEXT,
        status TEXT NOT NULL DEFAULT 'pending',
        started_at INTEGER NOT NULL,
        completed_at INTEGER,
        duration_ms INTEGER
      )
    `);

    // Create indexes for efficient queries
    await this.db.exec(`
      CREATE INDEX IF NOT EXISTS idx_tool_calls_name
      ON tool_calls(name)
    `);

    await this.db.exec(`
      CREATE INDEX IF NOT EXISTS idx_tool_calls_started_at
      ON tool_calls(started_at)
    `);
  }

  /**
   * Start a new tool call and mark it as pending
   * Returns the ID of the created tool call record
   */
  async start(name: string, parameters?: any): Promise<number> {
    const serializedParams = parameters !== undefined ? JSON.stringify(parameters) : null;
    const started_at = Math.floor(Date.now() / 1000);

    const stmt = await this.db.prepare(`
      INSERT INTO tool_calls (name, parameters, status, started_at)
      VALUES (?, ?, 'pending', ?)
      RETURNING id
    `);

    const { id } = await stmt.get(name, serializedParams, started_at);
    return Number(id);
  }

  /**
   * Mark a tool call as successful
   */
  async success(id: number, result?: any): Promise<void> {
    const serializedResult = result !== undefined ? JSON.stringify(result) : null;
    const completed_at = Math.floor(Date.now() / 1000);

    // Get the started_at time to calculate duration
    const getStmt = await this.db.prepare('SELECT started_at FROM tool_calls WHERE id = ?');
    const row = await getStmt.get(id) as { started_at: number } | undefined;

    if (!row) {
      throw new Error(`Tool call with ID ${id} not found`);
    }

    const duration_ms = (completed_at - row.started_at) * 1000;

    const updateStmt = await this.db.prepare(`
      UPDATE tool_calls
      SET status = 'success', result = ?, completed_at = ?, duration_ms = ?
      WHERE id = ?
    `);

    await updateStmt.run(serializedResult, completed_at, duration_ms, id);
  }

  /**
   * Mark a tool call as failed
   */
  async error(id: number, error: string): Promise<void> {
    const completed_at = Math.floor(Date.now() / 1000);

    // Get the started_at time to calculate duration
    const getStmt = await this.db.prepare('SELECT started_at FROM tool_calls WHERE id = ?');
    const row = await getStmt.get(id) as { started_at: number } | undefined;

    if (!row) {
      throw new Error(`Tool call with ID ${id} not found`);
    }

    const duration_ms = (completed_at - row.started_at) * 1000;

    const updateStmt = await this.db.prepare(`
      UPDATE tool_calls
      SET status = 'error', error = ?, completed_at = ?, duration_ms = ?
      WHERE id = ?
    `);

    await updateStmt.run(error, completed_at, duration_ms, id);
  }

  /**
   * Record a completed tool call
   * Either result or error should be provided, not both
   * Returns the ID of the created tool call record
   */
  async record(
    name: string,
    started_at: number,
    completed_at: number,
    parameters?: any,
    result?: any,
    error?: string
  ): Promise<number> {
    const serializedParams = parameters !== undefined ? JSON.stringify(parameters) : null;
    const serializedResult = result !== undefined ? JSON.stringify(result) : null;
    const duration_ms = (completed_at - started_at) * 1000;
    const status = error ? 'error' : 'success';

    const stmt = await this.db.prepare(`
      INSERT INTO tool_calls (name, parameters, result, error, status, started_at, completed_at, duration_ms)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?)
      RETURNING id
    `);

    const { id } = await stmt.get(name, serializedParams, serializedResult, error || null, status, started_at, completed_at, duration_ms);
    return Number(id);
  }

  /**
   * Get a specific tool call by ID
   */
  async get(id: number): Promise<ToolCall | undefined> {
    const stmt = await this.db.prepare(`
      SELECT * FROM tool_calls WHERE id = ?
    `);

    const row = await stmt.get(id) as any;
    if (!row) {
      return undefined;
    }

    return this.rowToToolCall(row);
  }

  /**
   * Query tool calls by name
   */
  async getByName(name: string, limit?: number): Promise<ToolCall[]> {
    const limitClause = limit !== undefined ? `LIMIT ${limit}` : '';
    const stmt = await this.db.prepare(`
      SELECT * FROM tool_calls
      WHERE name = ?
      ORDER BY started_at DESC
      ${limitClause}
    `);

    const rows = await stmt.all(name) as any[];
    return rows.map(row => this.rowToToolCall(row));
  }

  /**
   * Query recent tool calls
   */
  async getRecent(since: number, limit?: number): Promise<ToolCall[]> {
    const limitClause = limit !== undefined ? `LIMIT ${limit}` : '';
    const stmt = await this.db.prepare(`
      SELECT * FROM tool_calls
      WHERE started_at > ?
      ORDER BY started_at DESC
      ${limitClause}
    `);

    const rows = await stmt.all(since) as any[];
    return rows.map(row => this.rowToToolCall(row));
  }

  /**
   * Get performance statistics for all tools
   * Only includes completed calls (success or failed), not pending ones
   */
  async getStats(): Promise<ToolCallStats[]> {
    const stmt = await this.db.prepare(`
      SELECT
        name,
        COUNT(*) as total_calls,
        SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END) as successful,
        SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) as failed,
        AVG(duration_ms) as avg_duration_ms
      FROM tool_calls
      WHERE status != 'pending'
      GROUP BY name
      ORDER BY total_calls DESC
    `);

    const rows = await stmt.all() as any[];
    return rows.map(row => ({
      name: row.name,
      total_calls: row.total_calls,
      successful: row.successful,
      failed: row.failed,
      avg_duration_ms: row.avg_duration_ms || 0,
    }));
  }

  /**
   * Helper to convert database row to ToolCall object
   */
  private rowToToolCall(row: any): ToolCall {
    return {
      id: row.id,
      name: row.name,
      parameters: row.parameters !== null ? JSON.parse(row.parameters) : undefined,
      result: row.result !== null ? JSON.parse(row.result) : undefined,
      error: row.error !== null ? row.error : undefined,
      status: row.status as 'pending' | 'success' | 'error',
      started_at: row.started_at,
      completed_at: row.completed_at !== null ? row.completed_at : undefined,
      duration_ms: row.duration_ms !== null ? row.duration_ms : undefined,
    };
  }
}
