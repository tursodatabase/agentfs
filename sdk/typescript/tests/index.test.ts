import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { AgentFS, agentfsDir } from "../src/index";
import { existsSync, rmSync } from "fs";
import { join } from "path";

describe("AgentFS Integration Tests", () => {
  let agent: AgentFS;
  const testId = "test-agent";

  beforeEach(async () => {
    // Initialize AgentFS with a test id
    agent = await AgentFS.open({ id: testId });
  });

  afterEach(async () => {
    // Close the agent
    await agent.close();
    // Clean up test database files
    cleanupAgentFiles(testId);
  });

  describe("Initialization", () => {
    it("should successfully initialize with an id", async () => {
      expect(agent).toBeDefined();
      expect(agent).toBeInstanceOf(AgentFS);
    });

    it("should initialize with explicit path", async () => {
      const pathAgent = await AgentFS.open({ path: ":memory:" });
      expect(pathAgent).toBeDefined();
      expect(pathAgent).toBeInstanceOf(AgentFS);
      await pathAgent.close();
    });

    it("should require at least id or path", async () => {
      // @ts-expect-error - Testing runtime validation for JS users
      await expect(AgentFS.open({})).rejects.toThrow(
        "AgentFS.open() requires at least 'id' or 'path'"
      );
    });

    it("should allow multiple instances with different ids", async () => {
      const agent2 = await AgentFS.open({ id: "test-agent-2" });

      expect(agent).toBeDefined();
      expect(agent2).toBeDefined();
      expect(agent).not.toBe(agent2);

      await agent2.close();
      // Clean up second agent's database
      cleanupAgentFiles("test-agent-2");
    });
  });

  describe("Database Persistence", () => {
    it("should persist database file to global ~/.agentfs/fs directory", async () => {
      // Check that database file exists in global ~/.agentfs/fs directory
      const dbPath = join(agentfsDir(), `${testId}.db`);
      expect(existsSync(dbPath)).toBe(true);
    });

    it("should reuse existing database file with same id", async () => {
      // Create first instance and write data
      const persistenceTestId = "persistence-test";
      const agent1 = await AgentFS.open({ id: persistenceTestId });
      await agent1.kv.set("test", "value1");
      await agent1.close();

      // Create second instance with same id - should be able to read the data
      const agent2 = await AgentFS.open({ id: persistenceTestId });
      const value = await agent2.kv.get("test");

      expect(agent1).toBeDefined();
      expect(agent2).toBeDefined();
      expect(value).toBe("value1");

      await agent2.close();

      // Clean up
      cleanupAgentFiles(persistenceTestId);
    });
  });
});

/**
 * Helper function to clean up agent database files and related SQLite files
 * @param id The agent ID to clean up
 */
function cleanupAgentFiles(id: string): void {
  const dbPath = join(agentfsDir(), `${id}.db`);
  try {
    // Remove database file and SQLite WAL files
    [dbPath, `${dbPath}-shm`, `${dbPath}-wal`].forEach((file) => {
      if (existsSync(file)) {
        rmSync(file, { force: true });
      }
    });
  } catch {
    // Ignore cleanup errors
  }
}
