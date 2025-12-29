import { describe, it, expect, beforeEach } from "vitest";
import { Database } from "@tursodatabase/database-wasm/vite";
import { AgentFS as Filesystem } from "../src/filesystem/agentfs.js";

import { Buffer } from "buffer";
globalThis.Buffer = Buffer;

describe("Filesystem Integration Tests", () => {
  let db: Database;
  let fs: Filesystem;

  beforeEach(async () => {
    // Initialize database and Filesystem
    db = new Database(":memory:");
    await db.connect();
    // sync uses CDC so we must ensure that AgentFS components works properly with this extra setup
    await db.exec("PRAGMA unstable_capture_data_changes_conn('full')");
    fs = await Filesystem.fromDatabase(db);
  });

  describe("File Write Operations", () => {
    it("should write and read a simple text file", async () => {
      await fs.writeFile("/test.txt", "Hello, World!");
      const content = await fs.readFile("/test.txt", "utf8");
      expect(content).toBe("Hello, World!");
    });

    it("should write and read files in subdirectories", async () => {
      await fs.writeFile("/dir/subdir/file.txt", "nested content");
      const content = await fs.readFile("/dir/subdir/file.txt", "utf8");
      expect(content).toBe("nested content");
    });

    it("should overwrite existing file", async () => {
      await fs.writeFile("/overwrite.txt", "original content");
      await fs.writeFile("/overwrite.txt", "new content");
      const content = await fs.readFile("/overwrite.txt", "utf8");
      expect(content).toBe("new content");
    });

    it("should handle empty file content", async () => {
      await fs.writeFile("/empty.txt", "");
      const content = await fs.readFile("/empty.txt", "utf8");
      expect(content).toBe("");
    });

    it("should handle large file content", async () => {
      const largeContent = "x".repeat(100000);
      await fs.writeFile("/large.txt", largeContent);
      const content = await fs.readFile("/large.txt", "utf8");
      expect(content).toBe(largeContent);
    });

    it("should handle files with special characters in content", async () => {
      const specialContent = "Special chars: \n\t\r\"'\\";
      await fs.writeFile("/special.txt", specialContent);
      const content = await fs.readFile("/special.txt", "utf8");
      expect(content).toBe(specialContent);
    });
  });

  describe("File Read Operations", () => {
    it("should throw error when reading non-existent file", async () => {
      await expect(fs.readFile("/non-existent.txt")).rejects.toThrow();
    });

    it("should read multiple different files", async () => {
      await fs.writeFile("/file1.txt", "content 1");
      await fs.writeFile("/file2.txt", "content 2");
      await fs.writeFile("/file3.txt", "content 3");

      expect(await fs.readFile("/file1.txt", "utf8")).toBe("content 1");
      expect(await fs.readFile("/file2.txt", "utf8")).toBe("content 2");
      expect(await fs.readFile("/file3.txt", "utf8")).toBe("content 3");
    });
  });

  describe("Directory Operations", () => {
    it("should list files in root directory", async () => {
      await fs.writeFile("/file1.txt", "content 1");
      await fs.writeFile("/file2.txt", "content 2");
      await fs.writeFile("/file3.txt", "content 3");

      const files = await fs.readdir("/");
      expect(files).toContain("file1.txt");
      expect(files).toContain("file2.txt");
      expect(files).toContain("file3.txt");
      expect(files).toHaveLength(3);
    });

    it("should list files in subdirectory", async () => {
      await fs.writeFile("/dir/file1.txt", "content 1");
      await fs.writeFile("/dir/file2.txt", "content 2");
      await fs.writeFile("/other/file3.txt", "content 3");

      const files = await fs.readdir("/dir");
      expect(files).toContain("file1.txt");
      expect(files).toContain("file2.txt");
      expect(files).not.toContain("file3.txt");
      expect(files).toHaveLength(2);
    });

    it("should return empty array for empty directory", async () => {
      await fs.writeFile("/dir/file.txt", "content");
      // /dir exists but is not empty, root exists and should be empty except for 'dir'
      const files = await fs.readdir("/");
      expect(files).toContain("dir");
    });

    it("should distinguish between files in different directories", async () => {
      await fs.writeFile("/dir1/file.txt", "content 1");
      await fs.writeFile("/dir2/file.txt", "content 2");

      const files1 = await fs.readdir("/dir1");
      const files2 = await fs.readdir("/dir2");

      expect(files1).toContain("file.txt");
      expect(files2).toContain("file.txt");
      expect(files1).toHaveLength(1);
      expect(files2).toHaveLength(1);
    });

    it("should list subdirectories within a directory", async () => {
      await fs.writeFile("/parent/child1/file.txt", "content");
      await fs.writeFile("/parent/child2/file.txt", "content");
      await fs.writeFile("/parent/file.txt", "content");

      const entries = await fs.readdir("/parent");
      expect(entries).toContain("file.txt");
      expect(entries).toContain("child1");
      expect(entries).toContain("child2");
    });

    it("should handle nested directory structures", async () => {
      await fs.writeFile("/a/b/c/d/file.txt", "deep content");
      const files = await fs.readdir("/a/b/c/d");
      expect(files).toContain("file.txt");
    });
  });

  describe("File Delete Operations", () => {
    it("should delete an existing file", async () => {
      await fs.writeFile("/delete-me.txt", "content");
      await fs.deleteFile("/delete-me.txt");
      await expect(fs.readFile("/delete-me.txt")).rejects.toThrow();
    });

    it("should handle deleting non-existent file", async () => {
      await expect(fs.deleteFile("/non-existent.txt")).rejects.toThrow(
        "ENOENT"
      );
    });

    it("should delete file and update directory listing", async () => {
      await fs.writeFile("/dir/file1.txt", "content 1");
      await fs.writeFile("/dir/file2.txt", "content 2");

      await fs.deleteFile("/dir/file1.txt");

      const files = await fs.readdir("/dir");
      expect(files).not.toContain("file1.txt");
      expect(files).toContain("file2.txt");
      expect(files).toHaveLength(1);
    });

    it("should allow recreating deleted file", async () => {
      await fs.writeFile("/recreate.txt", "original");
      await fs.deleteFile("/recreate.txt");
      await fs.writeFile("/recreate.txt", "new content");
      const content = await fs.readFile("/recreate.txt", "utf8");
      expect(content).toBe("new content");
    });
  });

  describe("Path Handling", () => {
    it("should handle paths with trailing slashes", async () => {
      await fs.writeFile("/dir/file.txt", "content");
      const files1 = await fs.readdir("/dir");
      const files2 = await fs.readdir("/dir/");
      expect(files1).toEqual(files2);
    });

    it("should handle paths with special characters", async () => {
      const specialPath = "/dir-with-dash/file_with_underscore.txt";
      await fs.writeFile(specialPath, "content");
      const content = await fs.readFile(specialPath, "utf8");
      expect(content).toBe("content");
    });
  });

  describe("Concurrent Operations", () => {
    it("should handle concurrent writes to different files", async () => {
      const operations = Array.from({ length: 10 }, (_, i) =>
        fs.writeFile(`/concurrent-${i}.txt`, `content ${i}`)
      );
      await Promise.all(operations);

      // Verify all files were created
      for (let i = 0; i < 10; i++) {
        const content = await fs.readFile(`/concurrent-${i}.txt`, "utf8");
        expect(content).toBe(`content ${i}`);
      }
    });

    it("should handle concurrent reads", async () => {
      await fs.writeFile("/concurrent-read.txt", "shared content");

      const results = await Promise.all(
        Array.from({ length: 10 }, () =>
          fs.readFile("/concurrent-read.txt", "utf8")
        )
      );

      results.forEach((content) => {
        expect(content).toBe("shared content");
      });
    });
  });

  describe("File System Integrity", () => {
    it("should maintain file hierarchy integrity", async () => {
      await fs.writeFile("/root.txt", "root");
      await fs.writeFile("/dir1/file.txt", "dir1");
      await fs.writeFile("/dir2/file.txt", "dir2");
      await fs.writeFile("/dir1/subdir/file.txt", "subdir");

      expect(await fs.readFile("/root.txt", "utf8")).toBe("root");
      expect(await fs.readFile("/dir1/file.txt", "utf8")).toBe("dir1");
      expect(await fs.readFile("/dir2/file.txt", "utf8")).toBe("dir2");
      expect(await fs.readFile("/dir1/subdir/file.txt", "utf8")).toBe("subdir");

      const rootFiles = await fs.readdir("/");
      expect(rootFiles).toContain("root.txt");
      expect(rootFiles).toContain("dir1");
      expect(rootFiles).toContain("dir2");
    });

    it("should support multiple files with same name in different directories", async () => {
      await fs.writeFile("/dir1/config.json", '{"version": 1}');
      await fs.writeFile("/dir2/config.json", '{"version": 2}');

      expect(await fs.readFile("/dir1/config.json", "utf8")).toBe(
        '{"version": 1}'
      );
      expect(await fs.readFile("/dir2/config.json", "utf8")).toBe(
        '{"version": 2}'
      );
    });
  });

  describe("Standalone Usage", () => {
    it("should work with in-memory database when no db provided", async () => {
      const standaloneDb = new Database(":memory:");
      await standaloneDb.connect();
      const standaloneFs = await Filesystem.fromDatabase(standaloneDb);
      await standaloneFs.writeFile("/test.txt", "standalone content");
      const content = await standaloneFs.readFile("/test.txt", "utf8");
      expect(content).toBe("standalone content");
      await standaloneDb.close();
    });

    it("should maintain isolation between instances", async () => {
      const db1 = new Database(":memory:");
      await db1.connect();
      const fs1 = await Filesystem.fromDatabase(db1);

      const db2 = new Database(":memory:");
      await db2.connect();
      const fs2 = await Filesystem.fromDatabase(db2);

      await fs1.writeFile("/test.txt", "fs1 content");
      await fs2.writeFile("/test.txt", "fs2 content");

      expect(await fs1.readFile("/test.txt", "utf8")).toBe("fs1 content");
      expect(await fs2.readFile("/test.txt", "utf8")).toBe("fs2 content");

      await db1.close();
      await db2.close();
    });
  });

  describe("Persistence", () => {
    it("should persist data across Filesystem instances", async () => {
      await fs.writeFile("/persist.txt", "persistent content");
      const newFs = await Filesystem.fromDatabase(db);
      const content = await newFs.readFile("/persist.txt", "utf8");
      expect(content).toBe("persistent content");
    });
  });

  // ==================== Chunk Size Boundary Tests ====================

  describe("Chunk Size Boundary Tests", () => {
    // Helper function to get chunk count for an inode
    async function getChunkCount(path: string): Promise<number> {
      const stmt = db.prepare(`
        SELECT COUNT(*) as count FROM fs_data
        WHERE ino = (SELECT ino FROM fs_dentry WHERE parent_ino = 1 AND name = ?)
      `);
      const pathParts = path.split("/").filter((p) => p);
      const name = pathParts[pathParts.length - 1];

      // For simple paths, get the inode from the path
      const lookupStmt = db.prepare(`
        SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
      `);

      let currentIno = 1; // root
      for (const part of pathParts) {
        const result = (await lookupStmt.get(currentIno, part)) as
          | { ino: number }
          | undefined;
        if (!result) return 0;
        currentIno = result.ino;
      }

      const countStmt = db.prepare(
        "SELECT COUNT(*) as count FROM fs_data WHERE ino = ?"
      );
      const result = (await countStmt.get(currentIno)) as { count: number };
      return result.count;
    }

    it("should write file smaller than chunk size", async () => {
      // Write a file smaller than chunk_size (100 bytes)
      const data = "x".repeat(100);
      await fs.writeFile("/small.txt", data);

      // Read it back
      const readData = await fs.readFile("/small.txt", "utf8");
      expect(readData.length).toBe(100);
      expect(readData).toBe(data);

      // Verify only 1 chunk was created
      const chunkCount = await getChunkCount("/small.txt");
      expect(chunkCount).toBe(1);
    });

    it("should write file exactly chunk size", async () => {
      const chunkSize = fs.getChunkSize();
      // Write exactly chunk_size bytes
      const data = Buffer.alloc(chunkSize);
      for (let i = 0; i < chunkSize; i++) {
        data[i] = i % 256;
      }
      await fs.writeFile("/exact.txt", data);

      // Read it back
      const readData = (await fs.readFile("/exact.txt")) as Buffer;
      expect(readData.length).toBe(chunkSize);

      // Verify only 1 chunk was created
      const chunkCount = await getChunkCount("/exact.txt");
      expect(chunkCount).toBe(1);
    });

    it("should write file one byte over chunk size", async () => {
      const chunkSize = fs.getChunkSize();
      // Write chunk_size + 1 bytes
      const data = Buffer.alloc(chunkSize + 1);
      for (let i = 0; i <= chunkSize; i++) {
        data[i] = i % 256;
      }
      await fs.writeFile("/overflow.txt", data);

      // Read it back
      const readData = (await fs.readFile("/overflow.txt")) as Buffer;
      expect(readData.length).toBe(chunkSize + 1);

      // Verify 2 chunks were created
      const chunkCount = await getChunkCount("/overflow.txt");
      expect(chunkCount).toBe(2);
    });

    it("should write file spanning multiple chunks", async () => {
      const chunkSize = fs.getChunkSize();
      // Write ~2.5 chunks worth of data
      const dataSize = Math.floor(chunkSize * 2.5);
      const data = Buffer.alloc(dataSize);
      for (let i = 0; i < dataSize; i++) {
        data[i] = i % 256;
      }
      await fs.writeFile("/multi.txt", data);

      // Read it back
      const readData = (await fs.readFile("/multi.txt")) as Buffer;
      expect(readData.length).toBe(dataSize);

      // Verify 3 chunks were created
      const chunkCount = await getChunkCount("/multi.txt");
      expect(chunkCount).toBe(3);
    });
  });

  // ==================== Data Integrity Tests ====================

  describe("Data Integrity Tests", () => {
    async function getChunkCount(path: string): Promise<number> {
      const pathParts = path.split("/").filter((p) => p);
      const lookupStmt = db.prepare(`
        SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
      `);

      let currentIno = 1;
      for (const part of pathParts) {
        const result = (await lookupStmt.get(currentIno, part)) as
          | { ino: number }
          | undefined;
        if (!result) return 0;
        currentIno = result.ino;
      }

      const countStmt = db.prepare(
        "SELECT COUNT(*) as count FROM fs_data WHERE ino = ?"
      );
      const result = (await countStmt.get(currentIno)) as { count: number };
      return result.count;
    }

    it("should roundtrip data byte-for-byte", async () => {
      const chunkSize = fs.getChunkSize();
      // Create data that spans chunk boundaries with identifiable patterns
      const dataSize = chunkSize * 3 + 123; // Odd size spanning 4 chunks

      const data = Buffer.alloc(dataSize);
      for (let i = 0; i < dataSize; i++) {
        data[i] = i % 256;
      }
      await fs.writeFile("/roundtrip.bin", data);

      const readData = (await fs.readFile("/roundtrip.bin")) as Buffer;
      expect(readData.length).toBe(dataSize);

      // Verify chunk count
      const chunkCount = await getChunkCount("/roundtrip.bin");
      expect(chunkCount).toBe(4);
    });

    it("should handle binary data with null bytes", async () => {
      const chunkSize = fs.getChunkSize();
      // Create data with null bytes at chunk boundaries
      const data = Buffer.alloc(chunkSize * 2 + 100);
      // Put nulls at the chunk boundary
      data[chunkSize - 1] = 0;
      data[chunkSize] = 0;
      data[chunkSize + 1] = 0;
      // Put some non-null bytes around
      data[chunkSize - 2] = 0xff;
      data[chunkSize + 2] = 0xff;

      await fs.writeFile("/nulls.bin", data);
      const readData = (await fs.readFile("/nulls.bin")) as Buffer;

      expect(readData[chunkSize - 2]).toBe(0xff);
      expect(readData[chunkSize - 1]).toBe(0);
      expect(readData[chunkSize]).toBe(0);
      expect(readData[chunkSize + 1]).toBe(0);
      expect(readData[chunkSize + 2]).toBe(0xff);
    });

    it("should preserve chunk ordering", async () => {
      const chunkSize = fs.getChunkSize();
      // Create sequential bytes spanning multiple chunks
      const dataSize = chunkSize * 5;
      const data = Buffer.alloc(dataSize);
      for (let i = 0; i < dataSize; i++) {
        data[i] = i % 256;
      }
      await fs.writeFile("/sequential.bin", data);

      const readData = (await fs.readFile("/sequential.bin")) as Buffer;

      // Verify every byte is in the correct position
      for (let i = 0; i < dataSize; i++) {
        expect(readData[i]).toBe(i % 256);
      }
    });
  });

  // ==================== Edge Case Tests ====================

  describe("Edge Case Tests", () => {
    async function getChunkCount(path: string): Promise<number> {
      const pathParts = path.split("/").filter((p) => p);
      const lookupStmt = db.prepare(`
        SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
      `);

      let currentIno = 1;
      for (const part of pathParts) {
        const result = (await lookupStmt.get(currentIno, part)) as
          | { ino: number }
          | undefined;
        if (!result) return 0;
        currentIno = result.ino;
      }

      const countStmt = db.prepare(
        "SELECT COUNT(*) as count FROM fs_data WHERE ino = ?"
      );
      const result = (await countStmt.get(currentIno)) as { count: number };
      return result.count;
    }

    async function getIno(path: string): Promise<number> {
      const pathParts = path.split("/").filter((p) => p);
      const lookupStmt = db.prepare(`
        SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
      `);

      let currentIno = 1;
      for (const part of pathParts) {
        const result = (await lookupStmt.get(currentIno, part)) as
          | { ino: number }
          | undefined;
        if (!result) return 0;
        currentIno = result.ino;
      }
      return currentIno;
    }

    it("should handle empty file with zero chunks", async () => {
      // Write empty file
      await fs.writeFile("/empty.txt", "");

      // Read it back
      const readData = await fs.readFile("/empty.txt", "utf8");
      expect(readData).toBe("");

      // Verify 0 chunks were created
      const chunkCount = await getChunkCount("/empty.txt");
      expect(chunkCount).toBe(0);

      // Verify size is 0
      const stats = await fs.stat("/empty.txt");
      expect(stats.size).toBe(0);
    });

    it("should overwrite large file with smaller file and clean up chunks", async () => {
      const chunkSize = fs.getChunkSize();

      // Write initial large file (3 chunks)
      const initialData = Buffer.alloc(chunkSize * 3);
      for (let i = 0; i < chunkSize * 3; i++) {
        initialData[i] = i % 256;
      }
      await fs.writeFile("/overwrite.txt", initialData);

      const ino = await getIno("/overwrite.txt");
      const initialChunkCount = await getChunkCount("/overwrite.txt");
      expect(initialChunkCount).toBe(3);

      // Overwrite with smaller file (1 chunk)
      const newData = "x".repeat(100);
      await fs.writeFile("/overwrite.txt", newData);

      // Verify old chunks are gone and new data is correct
      const readData = await fs.readFile("/overwrite.txt", "utf8");
      expect(readData).toBe(newData);

      const newChunkCount = await getChunkCount("/overwrite.txt");
      expect(newChunkCount).toBe(1);

      // Verify size is updated
      const stats = await fs.stat("/overwrite.txt");
      expect(stats.size).toBe(100);
    });

    it("should overwrite small file with larger file", async () => {
      const chunkSize = fs.getChunkSize();

      // Write initial small file (1 chunk)
      const initialData = "x".repeat(100);
      await fs.writeFile("/grow.txt", initialData);

      expect(await getChunkCount("/grow.txt")).toBe(1);

      // Overwrite with larger file (3 chunks)
      const newData = Buffer.alloc(chunkSize * 3);
      for (let i = 0; i < chunkSize * 3; i++) {
        newData[i] = i % 256;
      }
      await fs.writeFile("/grow.txt", newData);

      // Verify data is correct (no encoding = Buffer)
      const readData = (await fs.readFile("/grow.txt")) as Buffer;
      expect(readData.length).toBe(chunkSize * 3);
      expect(await getChunkCount("/grow.txt")).toBe(3);
    });

    it("should handle very large file (1MB)", async () => {
      // Write 1MB file
      const dataSize = 1024 * 1024;
      const data = Buffer.alloc(dataSize);
      for (let i = 0; i < dataSize; i++) {
        data[i] = i % 256;
      }
      await fs.writeFile("/large.bin", data);

      const readData = (await fs.readFile("/large.bin")) as Buffer;
      expect(readData.length).toBe(dataSize);

      // Verify correct number of chunks
      const chunkSize = fs.getChunkSize();
      const expectedChunks = Math.ceil(dataSize / chunkSize);
      const actualChunks = await getChunkCount("/large.bin");
      expect(actualChunks).toBe(expectedChunks);
    });
  });

  // ==================== Configuration Tests ====================

  describe("Configuration Tests", () => {
    it("should have default chunk size of 4096", async () => {
      expect(fs.getChunkSize()).toBe(4096);
    });

    it("should verify chunk_size accessor works correctly", async () => {
      const chunkSize = fs.getChunkSize();
      expect(chunkSize).toBeGreaterThan(0);

      // Write data and verify chunks match expected based on chunk_size
      const data = Buffer.alloc(chunkSize * 2 + 1);
      await fs.writeFile("/test.bin", data);

      const pathParts = "/test.bin".split("/").filter((p) => p);
      const lookupStmt = db.prepare(`
        SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
      `);
      let currentIno = 1;
      for (const part of pathParts) {
        const result = (await lookupStmt.get(currentIno, part)) as
          | { ino: number }
          | undefined;
        if (result) currentIno = result.ino;
      }
      const countStmt = db.prepare(
        "SELECT COUNT(*) as count FROM fs_data WHERE ino = ?"
      );
      const result = (await countStmt.get(currentIno)) as { count: number };
      expect(result.count).toBe(3);
    });

    it("should persist chunk_size in fs_config table", async () => {
      // Query fs_config table directly
      const stmt = db.prepare(
        "SELECT value FROM fs_config WHERE key = 'chunk_size'"
      );
      const result = (await stmt.get()) as { value: string } | undefined;

      expect(result).toBeDefined();
      expect(result!.value).toBe("4096");
    });
  });

  // ==================== Schema Tests ====================

  describe("Schema Tests", () => {
    it("should enforce chunk index uniqueness", async () => {
      const chunkSize = fs.getChunkSize();
      // Write a file to create chunks
      const data = Buffer.alloc(chunkSize * 2);
      await fs.writeFile("/unique.txt", data);

      const lookupStmt = db.prepare(`
        SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
      `);
      const result = (await lookupStmt.get(1, "unique.txt")) as { ino: number };
      const ino = result.ino;

      // Try to insert a duplicate chunk - should fail due to PRIMARY KEY constraint
      const insertStmt = db.prepare(
        "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, 0, ?)"
      );

      await expect(
        insertStmt.run(ino, Buffer.from([1, 2, 3]))
      ).rejects.toThrow();
    });

    it("should store chunks with correct ordering in database", async () => {
      const chunkSize = fs.getChunkSize();
      // Create 5 chunks with identifiable data
      const dataSize = chunkSize * 5;
      const data = Buffer.alloc(dataSize);
      for (let i = 0; i < dataSize; i++) {
        data[i] = i % 256;
      }
      await fs.writeFile("/ordered.bin", data);

      const lookupStmt = db.prepare(`
        SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
      `);
      const result = (await lookupStmt.get(1, "ordered.bin")) as {
        ino: number;
      };
      const ino = result.ino;

      // Query chunks in order
      const queryStmt = db.prepare(
        "SELECT chunk_index FROM fs_data WHERE ino = ? ORDER BY chunk_index"
      );
      const rows = (await queryStmt.all(ino)) as { chunk_index: number }[];

      const indices = rows.map((r) => r.chunk_index);
      expect(indices).toEqual([0, 1, 2, 3, 4]);
    });
  });

  // ==================== Cleanup Tests ====================

  describe("Cleanup Tests", () => {
    it("should delete all chunks when file is removed", async () => {
      const chunkSize = fs.getChunkSize();
      // Create multi-chunk file
      const data = Buffer.alloc(chunkSize * 4);
      await fs.writeFile("/deleteme.txt", data);

      const lookupStmt = db.prepare(`
        SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
      `);
      const result = (await lookupStmt.get(1, "deleteme.txt")) as {
        ino: number;
      };
      const ino = result.ino;

      const countStmt = db.prepare(
        "SELECT COUNT(*) as count FROM fs_data WHERE ino = ?"
      );
      const beforeResult = (await countStmt.get(ino)) as { count: number };
      expect(beforeResult.count).toBe(4);

      // Delete the file
      await fs.deleteFile("/deleteme.txt");

      // Verify all chunks are gone
      const afterResult = (await countStmt.get(ino)) as { count: number };
      expect(afterResult.count).toBe(0);
    });

    it("should handle multiple files with different sizes correctly", async () => {
      const chunkSize = fs.getChunkSize();

      // Create files of various sizes
      const files: [string, number][] = [
        ["/tiny.txt", 10],
        ["/small.txt", Math.floor(chunkSize / 2)],
        ["/exact.txt", chunkSize],
        ["/medium.txt", chunkSize * 2 + 100],
        ["/large.txt", chunkSize * 5],
      ];

      for (const [path, size] of files) {
        const data = Buffer.alloc(size);
        for (let i = 0; i < size; i++) {
          data[i] = i % 256;
        }
        await fs.writeFile(path, data);
      }

      // Verify each file has correct data and chunk count
      for (const [path, size] of files) {
        const readData = (await fs.readFile(path)) as Buffer;
        expect(readData.length).toBe(size);

        const expectedChunks = size === 0 ? 0 : Math.ceil(size / chunkSize);

        const name = path.split("/").pop()!;
        const lookupStmt = db.prepare(`
          SELECT d.ino FROM fs_dentry d WHERE d.parent_ino = ? AND d.name = ?
        `);
        const result = (await lookupStmt.get(1, name)) as { ino: number };
        const countStmt = db.prepare(
          "SELECT COUNT(*) as count FROM fs_data WHERE ino = ?"
        );
        const countResult = (await countStmt.get(result.ino)) as {
          count: number;
        };

        expect(countResult.count).toBe(expectedChunks);
      }
    });
  });
});
