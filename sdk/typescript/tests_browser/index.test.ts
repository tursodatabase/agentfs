import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { Database } from "@tursodatabase/database-wasm/vite";
import { AgentFS } from "../";
import { Buffer } from "buffer";

describe("AgentFS Integration Tests", () => {
  let agent: AgentFS;
  beforeEach(async () => {
    const db = new Database(":memory:");
    await db.connect();
    agent = await AgentFS.openWith(db);
  });

  afterEach(async () => {
    await agent.close();
  });

  describe("Initialization", () => {
    it("should successfully initialize with an id", async () => {
      expect(agent).toBeDefined();
      expect(agent).toBeInstanceOf(AgentFS);
    });
    it("should polyfill Buffer automatically", async () => {
      const chunkSize = agent.fs.getChunkSize();
      // Create sequential bytes spanning multiple chunks
      const dataSize = chunkSize * 5;
      const data = Buffer.alloc(dataSize);
      for (let i = 0; i < dataSize; i++) {
        data[i] = i % 256;
      }
      await agent.fs.writeFile("/sequential.bin", data);

      const readData = (await agent.fs.readFile("/sequential.bin")) as Buffer;

      // Verify every byte is in the correct position
      for (let i = 0; i < dataSize; i++) {
        expect(readData[i]).toBe(i % 256);
      }
    });
  });
});
