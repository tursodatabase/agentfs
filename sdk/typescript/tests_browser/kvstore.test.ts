import { describe, it, expect, beforeEach } from "vitest";
import { Database } from "@tursodatabase/database-wasm/vite";
import { KvStore } from "../src/kvstore.js";

describe("KvStore Integration Tests", () => {
  let db: Database;
  let kvStore: KvStore;
  let tempDir: string;
  let dbPath: string;

  beforeEach(async () => {
    // Create temporary directory for test database
    db = new Database(":memory:");
    await db.connect();
    // sync uses CDC so we must ensure that AgentFS components works properly with this extra setup
    await db.exec("PRAGMA unstable_capture_data_changes_conn('full')");
    kvStore = await KvStore.fromDatabase(db);
  });

  describe("Basic Operations", () => {
    it("should set and get a string value", async () => {
      await kvStore.set("test-key", "test-value");
      const value = await kvStore.get("test-key");
      expect(value).toBe("test-value");
    });

    it("should set and get an object value", async () => {
      const testObject = { name: "test", count: 42, nested: { value: true } };
      await kvStore.set("object-key", testObject);
      const value = await kvStore.get("object-key");
      expect(value).toEqual(testObject);
    });

    it("should set and get a number value", async () => {
      await kvStore.set("number-key", 12345);
      const value = await kvStore.get("number-key");
      expect(value).toBe(12345);
    });

    it("should set and get a boolean value", async () => {
      await kvStore.set("bool-key", true);
      const value = await kvStore.get("bool-key");
      expect(value).toBe(true);
    });

    it("should set and get an array value", async () => {
      const testArray = [1, 2, "three", { four: 4 }];
      await kvStore.set("array-key", testArray);
      const value = await kvStore.get("array-key");
      expect(value).toEqual(testArray);
    });
    it("should set and list values", async () => {
      await kvStore.set("g1:k1", 1);
      await kvStore.set("g1:k2", 2);
      await kvStore.set("g2:k1", 3);
      await kvStore.set("g2:k2", 4);
      expect(await kvStore.list("g1:")).toEqual([
        { key: "g1:k1", value: 1 },
        { key: "g1:k2", value: 2 },
      ]);
      expect(await kvStore.list("g1:k1")).toEqual([{ key: "g1:k1", value: 1 }]);
      expect(await kvStore.list("g1:k3")).toEqual([]);
      expect(await kvStore.list("g2:")).toEqual([
        { key: "g2:k1", value: 3 },
        { key: "g2:k2", value: 4 },
      ]);
    });
  });

  describe("Update Operations", () => {
    it("should update an existing value", async () => {
      await kvStore.set("update-key", "initial-value");
      await kvStore.set("update-key", "updated-value");
      const value = await kvStore.get("update-key");
      expect(value).toBe("updated-value");
    });

    it("should update value type", async () => {
      await kvStore.set("type-key", "string-value");
      await kvStore.set("type-key", { object: "value" });
      const value = await kvStore.get("type-key");
      expect(value).toEqual({ object: "value" });
    });
  });

  describe("Delete Operations", () => {
    it("should delete an existing key", async () => {
      await kvStore.set("delete-key", "value-to-delete");
      await kvStore.delete("delete-key");
      const value = await kvStore.get("delete-key");
      expect(value).toBeUndefined();
    });

    it("should handle deleting non-existent key", async () => {
      // Should not throw an error when deleting a non-existent key
      await kvStore.delete("non-existent-key");
      // If we get here without throwing, the test passes
      expect(true).toBe(true);
    });
  });

  describe("Edge Cases", () => {
    it("should return undefined for non-existent key", async () => {
      const value = await kvStore.get("non-existent-key");
      expect(value).toBeUndefined();
    });

    it("should handle null values", async () => {
      await kvStore.set("null-key", null);
      const value = await kvStore.get("null-key");
      expect(value).toBeNull();
    });

    it("should handle empty string", async () => {
      await kvStore.set("empty-key", "");
      const value = await kvStore.get("empty-key");
      expect(value).toBe("");
    });

    it("should handle zero value", async () => {
      await kvStore.set("zero-key", 0);
      const value = await kvStore.get("zero-key");
      expect(value).toBe(0);
    });

    it("should handle keys with special characters", async () => {
      const specialKey = "key:with/special.chars@123";
      await kvStore.set(specialKey, "value");
      const value = await kvStore.get(specialKey);
      expect(value).toBe("value");
    });
  });

  describe("Concurrent Operations", () => {
    it("should handle multiple concurrent sets", async () => {
      const promises = Array.from({ length: 10 }, (_, i) =>
        kvStore.set(`concurrent-key-${i}`, `value-${i}`)
      );
      await Promise.all(promises);

      // Verify all values were set
      for (let i = 0; i < 10; i++) {
        const value = await kvStore.get(`concurrent-key-${i}`);
        expect(value).toBe(`value-${i}`);
      }
    });

    it("should handle concurrent reads", async () => {
      await kvStore.set("read-key", "read-value");

      const promises = Array.from({ length: 10 }, () =>
        kvStore.get("read-key")
      );
      const values = await Promise.all(promises);

      values.forEach((value) => {
        expect(value).toBe("read-value");
      });
    });
  });

  describe("Large Data", () => {
    it("should handle large string values", async () => {
      const largeString = "x".repeat(10000);
      await kvStore.set("large-string", largeString);
      const value = await kvStore.get("large-string");
      expect(value).toBe(largeString);
    });

    it("should handle large object values", async () => {
      const largeObject = {
        items: Array.from({ length: 1000 }, (_, i) => ({
          id: i,
          name: `Item ${i}`,
          data: `Data for item ${i}`,
        })),
      };
      await kvStore.set("large-object", largeObject);
      const value = await kvStore.get("large-object");
      expect(value).toEqual(largeObject);
    });
  });

  describe("Persistence", () => {
    it("should persist data across KvStore instances", async () => {
      await kvStore.set("persist-key", "persist-value");

      // Create new KvStore instance with same database
      const newKvStore = await KvStore.fromDatabase(db);
      const value = await newKvStore.get("persist-key");
      expect(value).toBe("persist-value");
    });
  });
});
