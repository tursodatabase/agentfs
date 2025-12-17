import { AgentFSCore } from "./agentfs.js";
import { DatabasePromise } from "@tursodatabase/database-common";
import { KvStore } from "./kvstore.js";
import { Filesystem } from "./filesystem.js";
import { ToolCalls } from "./toolcalls.js";
import { Buffer } from "buffer";

export class AgentFS extends AgentFSCore {
    static async openWith(db: DatabasePromise): Promise<AgentFSCore> {
        const [kv, fs, tools] = await Promise.all([
            KvStore.fromDatabase(db),
            Filesystem.fromDatabase(db, Buffer),
            ToolCalls.fromDatabase(db),
        ]);
        return new AgentFS(db, kv, fs, tools);
    }
}

export { AgentFSOptions } from './agentfs.js';
export { KvStore } from './kvstore.js';
export { Filesystem } from './filesystem.js';
export type { Stats } from './filesystem.js';
export { ToolCalls } from './toolcalls.js';
export type { ToolCall, ToolCallStats } from './toolcalls.js';