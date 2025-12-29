import { AgentFSCore } from "./agentfs.js";
import { DatabasePromise } from "@tursodatabase/database-common";
import { KvStore } from "./kvstore.js";
import { AgentFS as AgentFSImpl } from "./filesystem/index.js";
import { ToolCalls } from "./toolcalls.js";
import { Buffer } from "buffer";

export class AgentFS extends AgentFSCore {
    static async openWith(db: DatabasePromise): Promise<AgentFSCore> {
        const [kv, fs, tools] = await Promise.all([
            KvStore.fromDatabase(db),
            AgentFSImpl.fromDatabase(db, Buffer),
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