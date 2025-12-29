import { Database } from "@tursodatabase/database";
import { existsSync, mkdirSync } from "fs";
import { AgentFSCore, AgentFSOptions } from "./agentfs.js";
import { DatabasePromise } from "@tursodatabase/database-common";
import { KvStore } from "./kvstore.js";
import { AgentFS as Filesystem } from "./filesystem/index.js";
import { ToolCalls } from "./toolcalls.js";

export class AgentFS extends AgentFSCore {
    /**
   * Open an agent filesystem
   * @param options Configuration options (id and/or path required)
   * @returns Fully initialized AgentFS instance
   * @example
   * ```typescript
   * // Using id (creates .agentfs/my-agent.db)
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
            const dir = '.agentfs';
            if (!existsSync(dir)) {
                mkdirSync(dir, { recursive: true });
            }
            dbPath = `${dir}/${id}.db`;
        }

        const db = new Database(dbPath);

        // Connect to the database to ensure it's created
        await db.connect();

        return await this.openWith(db);
    }

    static async openWith(db: DatabasePromise): Promise<AgentFSCore> {
        const [kv, fs, tools] = await Promise.all([
            KvStore.fromDatabase(db),
            Filesystem.fromDatabase(db),
            ToolCalls.fromDatabase(db),
        ]);
        return new AgentFS(db, kv, fs, tools);
    }
}

export { AgentFSOptions } from './agentfs.js';
export { KvStore } from './kvstore.js';
export { AgentFS as Filesystem } from './filesystem/index.js';
export type { Stats, DirEntry, FilesystemStats, FileHandle, FileSystem } from './filesystem/index.js';
export { ToolCalls } from './toolcalls.js';
export type { ToolCall, ToolCallStats } from './toolcalls.js';
