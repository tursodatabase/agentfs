import { describe, it, expect, beforeEach } from "vitest";
import { Database } from "@tursodatabase/database-wasm/vite";
import { ToolCalls } from "../src/toolcalls.js";

describe("ToolCalls Integration Tests", () => {
  let db: Database;
  let toolCalls: ToolCalls;

  beforeEach(async () => {
    db = new Database(":memory:");
    await db.connect();
    // sync uses CDC so we must ensure that AgentFS components works properly with this extra setup
    await db.exec("PRAGMA unstable_capture_data_changes_conn('full')");
    toolCalls = await ToolCalls.fromDatabase(db);
  });

  describe("Basic Operations", () => {
    it("should start a tool call and return an ID", async () => {
      const id = await toolCalls.start("test_tool", { arg1: "value1" });
      expect(id).toBeGreaterThan(0);
    });

    it("should start a tool call without parameters", async () => {
      const id = await toolCalls.start("simple_tool");
      expect(id).toBeGreaterThan(0);

      const toolCall = await toolCalls.get(id);
      expect(toolCall).toBeDefined();
      expect(toolCall?.name).toBe("simple_tool");
      expect(toolCall?.parameters).toBeUndefined();
      expect(toolCall?.status).toBe("pending");
    });

    it("should mark a tool call as successful", async () => {
      const id = await toolCalls.start("test_tool", { input: "test" });
      await toolCalls.success(id, { output: "result" });

      const toolCall = await toolCalls.get(id);
      expect(toolCall).toBeDefined();
      expect(toolCall?.status).toBe("success");
      expect(toolCall?.result).toEqual({ output: "result" });
      expect(toolCall?.completed_at).toBeGreaterThan(0);
      expect(toolCall?.duration_ms).toBeGreaterThanOrEqual(0);
    });

    it("should mark a tool call as successful without result", async () => {
      const id = await toolCalls.start("test_tool", { input: "test" });
      await toolCalls.success(id);

      const toolCall = await toolCalls.get(id);
      expect(toolCall).toBeDefined();
      expect(toolCall?.status).toBe("success");
      expect(toolCall?.result).toBeUndefined();
    });

    it("should mark a tool call as failed", async () => {
      const id = await toolCalls.start("test_tool", { input: "test" });
      await toolCalls.error(id, "Something went wrong");

      const toolCall = await toolCalls.get(id);
      expect(toolCall).toBeDefined();
      expect(toolCall?.status).toBe("error");
      expect(toolCall?.error).toBe("Something went wrong");
      expect(toolCall?.completed_at).toBeGreaterThan(0);
      expect(toolCall?.duration_ms).toBeGreaterThanOrEqual(0);
    });

    it("should get a tool call by ID", async () => {
      const id = await toolCalls.start("test_tool", { arg: "value" });
      const toolCall = await toolCalls.get(id);

      expect(toolCall).toBeDefined();
      expect(toolCall?.id).toBe(id);
      expect(toolCall?.name).toBe("test_tool");
      expect(toolCall?.parameters).toEqual({ arg: "value" });
      expect(toolCall?.status).toBe("pending");
      expect(toolCall?.started_at).toBeGreaterThan(0);
    });

    it("should return undefined for non-existent ID", async () => {
      const toolCall = await toolCalls.get(99999);
      expect(toolCall).toBeUndefined();
    });
  });

  describe("Query Operations", () => {
    it("should get tool calls by name", async () => {
      await toolCalls.start("tool_a", { test: 1 });
      await toolCalls.start("tool_b", { test: 2 });
      await toolCalls.start("tool_a", { test: 3 });

      const results = await toolCalls.getByName("tool_a");
      expect(results).toHaveLength(2);
      expect(results.every((tc) => tc.name === "tool_a")).toBe(true);
    });

    it("should limit results when querying by name", async () => {
      await toolCalls.start("tool_a", { test: 1 });
      await toolCalls.start("tool_a", { test: 2 });
      await toolCalls.start("tool_a", { test: 3 });

      const results = await toolCalls.getByName("tool_a", 2);
      expect(results).toHaveLength(2);
    });

    it("should get recent tool calls", async () => {
      await toolCalls.start("tool_old");
      // Wait a full second to ensure different timestamps
      await new Promise((resolve) => setTimeout(resolve, 1100));
      const midpoint = Math.floor(Date.now() / 1000);
      // Wait a bit more to ensure next call is definitely after midpoint
      await new Promise((resolve) => setTimeout(resolve, 1100));
      await toolCalls.start("tool_new");

      const results = await toolCalls.getRecent(midpoint);
      expect(results.length).toBeGreaterThanOrEqual(1);
      expect(results.every((tc) => tc.started_at >= midpoint)).toBe(true);
    });

    it("should limit recent tool calls", async () => {
      const now = Math.floor(Date.now() / 1000);

      await toolCalls.start("tool_1");
      await toolCalls.start("tool_2");
      await toolCalls.start("tool_3");

      const results = await toolCalls.getRecent(now - 10, 2);
      expect(results.length).toBeLessThanOrEqual(2);
    });

    it("should return empty array when no matching tool calls by name", async () => {
      const results = await toolCalls.getByName("non_existent_tool");
      expect(results).toEqual([]);
    });
  });

  describe("Statistics", () => {
    it("should calculate tool call statistics", async () => {
      // Create some tool calls
      const id1 = await toolCalls.start("tool_a", { test: 1 });
      await toolCalls.success(id1, { result: "ok" });

      const id2 = await toolCalls.start("tool_a", { test: 2 });
      await toolCalls.error(id2, "failed");

      const id3 = await toolCalls.start("tool_a", { test: 3 });
      await toolCalls.success(id3, { result: "ok" });

      const id4 = await toolCalls.start("tool_b", { test: 4 });
      await toolCalls.success(id4, { result: "ok" });

      const stats = await toolCalls.getStats();

      expect(stats).toHaveLength(2);

      const toolAStats = stats.find((s) => s.name === "tool_a");
      expect(toolAStats).toBeDefined();
      expect(toolAStats?.total_calls).toBe(3);
      expect(toolAStats?.successful).toBe(2);
      expect(toolAStats?.failed).toBe(1);
      expect(toolAStats?.avg_duration_ms).toBeGreaterThanOrEqual(0);

      const toolBStats = stats.find((s) => s.name === "tool_b");
      expect(toolBStats).toBeDefined();
      expect(toolBStats?.total_calls).toBe(1);
      expect(toolBStats?.successful).toBe(1);
      expect(toolBStats?.failed).toBe(0);
    });

    it("should exclude pending calls from statistics", async () => {
      const id1 = await toolCalls.start("tool_a", { test: 1 });
      await toolCalls.success(id1, { result: "ok" });

      // This one stays pending
      await toolCalls.start("tool_a", { test: 2 });

      const stats = await toolCalls.getStats();
      const toolAStats = stats.find((s) => s.name === "tool_a");

      expect(toolAStats).toBeDefined();
      expect(toolAStats?.total_calls).toBe(1); // Only completed calls
    });

    it("should return empty array when no completed calls", async () => {
      await toolCalls.start("tool_a", { test: 1 });
      const stats = await toolCalls.getStats();
      expect(stats).toEqual([]);
    });
  });

  describe("Complex Parameters and Results", () => {
    it("should handle complex nested parameters", async () => {
      const complexParams = {
        user: { id: 123, name: "Test User" },
        options: { timeout: 5000, retry: true },
        data: [1, 2, 3, 4, 5],
      };

      const id = await toolCalls.start("complex_tool", complexParams);
      const toolCall = await toolCalls.get(id);

      expect(toolCall?.parameters).toEqual(complexParams);
    });

    it("should handle complex nested results", async () => {
      const complexResult = {
        data: {
          items: [
            { id: 1, value: "a" },
            { id: 2, value: "b" },
          ],
        },
        metadata: { count: 2, hasMore: false },
      };

      const id = await toolCalls.start("complex_tool");
      await toolCalls.success(id, complexResult);
      const toolCall = await toolCalls.get(id);

      expect(toolCall?.result).toEqual(complexResult);
    });

    it("should handle large parameters", async () => {
      const largeParams = {
        items: Array.from({ length: 100 }, (_, i) => ({
          id: i,
          data: `Data for item ${i}`,
        })),
      };

      const id = await toolCalls.start("large_tool", largeParams);
      const toolCall = await toolCalls.get(id);

      expect(toolCall?.parameters).toEqual(largeParams);
    });
  });

  describe("Concurrent Operations", () => {
    it("should handle multiple concurrent tool calls", async () => {
      const promises = Array.from({ length: 10 }, (_, i) =>
        toolCalls.start(`tool_${i}`, { index: i })
      );
      const ids = await Promise.all(promises);

      expect(ids).toHaveLength(10);
      expect(new Set(ids).size).toBe(10); // All IDs should be unique
    });

    it("should handle concurrent completions", async () => {
      const ids = await Promise.all([
        toolCalls.start("tool_1"),
        toolCalls.start("tool_2"),
        toolCalls.start("tool_3"),
      ]);

      await Promise.all([
        toolCalls.success(ids[0]),
        toolCalls.error(ids[1], "error"),
        toolCalls.success(ids[2]),
      ]);

      const results = await Promise.all(ids.map((id) => toolCalls.get(id)));

      expect(results[0]?.status).toBe("success");
      expect(results[1]?.status).toBe("error");
      expect(results[2]?.status).toBe("success");
    });
  });

  describe("Persistence", () => {
    it("should persist tool calls across instances", async () => {
      const id = await toolCalls.start("persist_tool", { test: "value" });
      await toolCalls.success(id, { result: "ok" });

      // Create new ToolCalls instance with same database
      const newToolCalls = await ToolCalls.fromDatabase(db);
      const toolCall = await newToolCalls.get(id);

      expect(toolCall).toBeDefined();
      expect(toolCall?.name).toBe("persist_tool");
      expect(toolCall?.status).toBe("success");
    });
  });

  describe("Ordering", () => {
    it("should return tool calls ordered by started_at desc", async () => {
      await toolCalls.start("tool_first");
      await new Promise((resolve) => setTimeout(resolve, 100));
      await toolCalls.start("tool_second");
      await new Promise((resolve) => setTimeout(resolve, 100));
      await toolCalls.start("tool_third");

      const recent = await toolCalls.getRecent(0);

      expect(recent.length).toBeGreaterThanOrEqual(3);
      // Most recent first
      for (let i = 0; i < recent.length - 1; i++) {
        expect(recent[i].started_at).toBeGreaterThanOrEqual(
          recent[i + 1].started_at
        );
      }
    });
  });
});
