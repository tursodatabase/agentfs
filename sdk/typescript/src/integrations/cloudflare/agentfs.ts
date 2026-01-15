/**
 * AgentFS - FileSystem implementation using Cloudflare Durable Objects SQLite
 *
 * This implementation uses Cloudflare's Durable Objects SQLite storage API,
 * allowing AgentFS to run on Cloudflare's edge platform.
 *
 * @see https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/
 */

import {
  S_IFMT,
  S_IFREG,
  S_IFDIR,
  S_IFLNK,
  DEFAULT_FILE_MODE,
  DEFAULT_DIR_MODE,
  createStats,
  type Stats,
  type DirEntry,
  type FilesystemStats,
  type FileHandle,
  type FileSystem,
} from "../../filesystem/interface.js";

const DEFAULT_CHUNK_SIZE = 4096;

/**
 * Cloudflare Durable Objects SqlStorage cursor interface
 */
interface SqlStorageCursor<T = Record<string, unknown>> {
  toArray(): T[];
  one(): T;
  raw(): IterableIterator<unknown[]>;
  readonly columnNames: string[];
  readonly rowsRead: number;
  readonly rowsWritten: number;
  next(): { done: boolean; value?: T };
  [Symbol.iterator](): IterableIterator<T>;
}

/**
 * Cloudflare Durable Objects SqlStorage interface
 */
interface SqlStorage {
  exec<T = Record<string, unknown>>(
    query: string,
    ...bindings: unknown[]
  ): SqlStorageCursor<T>;
  readonly databaseSize: number;
}

/**
 * Cloudflare Durable Objects Storage interface (subset we need)
 */
export interface CloudflareStorage {
  readonly sql: SqlStorage;
  transactionSync<T>(callback: () => T): T;
}

/**
 * Error codes for filesystem operations
 */
type FsErrorCode =
  | "ENOENT"
  | "EEXIST"
  | "EISDIR"
  | "ENOTDIR"
  | "ENOTEMPTY"
  | "EPERM"
  | "EINVAL";

interface FsError extends Error {
  code: FsErrorCode;
  syscall: string;
  path: string;
}

function createFsError(opts: {
  code: FsErrorCode;
  syscall: string;
  path: string;
  message: string;
}): FsError {
  const err = new Error(
    `${opts.code}: ${opts.message}, ${opts.syscall} '${opts.path}'`,
  ) as FsError;
  err.code = opts.code;
  err.syscall = opts.syscall;
  err.path = opts.path;
  return err;
}

/**
 * An open file handle for AgentFS.
 */
class AgentFSFile implements FileHandle {
  private storage: CloudflareStorage;
  private ino: number;
  private chunkSize: number;

  constructor(storage: CloudflareStorage, ino: number, chunkSize: number) {
    this.storage = storage;
    this.ino = ino;
    this.chunkSize = chunkSize;
  }

  async pread(offset: number, size: number): Promise<Buffer> {
    const startChunk = Math.floor(offset / this.chunkSize);
    const endChunk = Math.floor((offset + size - 1) / this.chunkSize);

    const rows = this.storage.sql
      .exec<{ chunk_index: number; data: ArrayBuffer }>(
        `SELECT chunk_index, data FROM fs_data
       WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ?
       ORDER BY chunk_index ASC`,
        this.ino,
        startChunk,
        endChunk,
      )
      .toArray();

    const buffers: Buffer[] = [];
    let bytesCollected = 0;
    const startOffsetInChunk = offset % this.chunkSize;

    for (const row of rows) {
      const skip = buffers.length === 0 ? startOffsetInChunk : 0;
      const data = Buffer.from(row.data);
      if (skip >= data.length) {
        continue;
      }
      const remaining = size - bytesCollected;
      const take = Math.min(data.length - skip, remaining);
      buffers.push(data.subarray(skip, skip + take));
      bytesCollected += take;
    }

    if (buffers.length === 0) {
      return Buffer.alloc(0);
    }

    return Buffer.concat(buffers);
  }

  async pwrite(offset: number, data: Buffer): Promise<void> {
    if (data.length === 0) {
      return;
    }

    const sizeRows = this.storage.sql
      .exec<{
        size: number;
      }>("SELECT size FROM fs_inode WHERE ino = ?", this.ino)
      .toArray();
    const currentSize = sizeRows[0]?.size ?? 0;

    if (offset > currentSize) {
      throw createFsError({
        code: "EINVAL",
        syscall: "pwrite",
        path: `ino ${this.ino}`,
        message: "offset past end of file",
      });
    }

    const newSize = Math.max(currentSize, offset + data.length);
    const now = Math.floor(Date.now() / 1000);
    this.storage.sql.exec(
      "UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?",
      newSize,
      now,
      this.ino,
    );

    let dataOffset = 0;
    while (dataOffset < data.length) {
      const currentOffset = offset + dataOffset;
      const chunkIdx = Math.floor(currentOffset / this.chunkSize);
      const chunkStart = chunkIdx * this.chunkSize;
      const chunkEnd = chunkStart + this.chunkSize;
      const dataStart = dataOffset;
      const dataEnd = Math.min(data.length, chunkEnd - offset);
      const writeOffset = Math.max(0, offset - chunkStart);

      const existingRows = this.storage.sql
        .exec<{
          data: ArrayBuffer;
        }>("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?", this.ino, chunkIdx)
        .toArray();

      let chunkData: Buffer;
      if (existingRows.length > 0) {
        chunkData = Buffer.from(existingRows[0].data);
        if (writeOffset + (dataEnd - dataStart) > chunkData.length) {
          const newChunk = Buffer.alloc(writeOffset + (dataEnd - dataStart));
          chunkData.copy(newChunk);
          chunkData = newChunk;
        }
      } else {
        chunkData = Buffer.alloc(writeOffset + (dataEnd - dataStart));
      }

      data.copy(chunkData, writeOffset, dataStart, dataEnd);

      this.storage.sql.exec(
        `INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)`,
        this.ino,
        chunkIdx,
        chunkData,
      );

      dataOffset = dataEnd;
    }
  }

  async truncate(newSize: number): Promise<void> {
    const sizeRows = this.storage.sql
      .exec<{
        size: number;
      }>("SELECT size FROM fs_inode WHERE ino = ?", this.ino)
      .toArray();
    const currentSize = sizeRows[0]?.size ?? 0;

    this.storage.transactionSync(() => {
      if (newSize === 0) {
        this.storage.sql.exec("DELETE FROM fs_data WHERE ino = ?", this.ino);
      } else if (newSize < currentSize) {
        const lastChunkIdx = Math.floor((newSize - 1) / this.chunkSize);

        this.storage.sql.exec(
          "DELETE FROM fs_data WHERE ino = ? AND chunk_index > ?",
          this.ino,
          lastChunkIdx,
        );

        const offsetInChunk = newSize % this.chunkSize;
        if (offsetInChunk > 0) {
          const rows = this.storage.sql
            .exec<{
              data: ArrayBuffer;
            }>("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?", this.ino, lastChunkIdx)
            .toArray();

          if (rows.length > 0 && rows[0].data.byteLength > offsetInChunk) {
            const truncatedChunk = Buffer.from(rows[0].data).subarray(
              0,
              offsetInChunk,
            );
            this.storage.sql.exec(
              "UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?",
              truncatedChunk,
              this.ino,
              lastChunkIdx,
            );
          }
        }
      }

      const now = Math.floor(Date.now() / 1000);
      this.storage.sql.exec(
        "UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?",
        newSize,
        now,
        this.ino,
      );
    });
  }

  async fsync(): Promise<void> {
    // Cloudflare Durable Objects SQLite is always synchronous
  }

  async fstat(): Promise<Stats> {
    const rows = this.storage.sql
      .exec<{
        ino: number;
        mode: number;
        nlink: number;
        uid: number;
        gid: number;
        size: number;
        atime: number;
        mtime: number;
        ctime: number;
      }>(
        `SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime
       FROM fs_inode WHERE ino = ?`,
        this.ino,
      )
      .toArray();

    if (rows.length === 0) {
      throw createFsError({
        code: "ENOENT",
        syscall: "fstat",
        path: `ino ${this.ino}`,
        message: "no such file or directory",
      });
    }

    return createStats(rows[0]);
  }

  async close(): Promise<void> {
    // No-op for synchronous storage
  }
}

/**
 * AgentFS implementation for Cloudflare Durable Objects
 */
export class AgentFS implements FileSystem {
  private storage: CloudflareStorage;
  private rootIno = 1;
  private chunkSize: number;

  constructor(storage: CloudflareStorage) {
    this.storage = storage;

    // Initialize schema and get chunk size
    this.initialize();
    this.chunkSize = this.ensureRoot();
  }

  private initialize(): void {
    this.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS fs_config (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      )
    `);

    this.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS fs_inode (
        ino INTEGER PRIMARY KEY,
        mode INTEGER NOT NULL,
        nlink INTEGER NOT NULL DEFAULT 1,
        uid INTEGER NOT NULL DEFAULT 0,
        gid INTEGER NOT NULL DEFAULT 0,
        size INTEGER NOT NULL DEFAULT 0,
        atime INTEGER NOT NULL,
        mtime INTEGER NOT NULL,
        ctime INTEGER NOT NULL
      )
    `);

    this.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS fs_dentry (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        parent_ino INTEGER NOT NULL,
        ino INTEGER NOT NULL,
        UNIQUE(parent_ino, name)
      )
    `);

    this.storage.sql.exec(`
      CREATE INDEX IF NOT EXISTS idx_fs_dentry_parent
      ON fs_dentry(parent_ino, name)
    `);

    this.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS fs_data (
        ino INTEGER NOT NULL,
        chunk_index INTEGER NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (ino, chunk_index)
      )
    `);

    this.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS fs_symlink (
        ino INTEGER PRIMARY KEY,
        target TEXT NOT NULL
      )
    `);
  }

  private ensureRoot(): number {
    const configRows = this.storage.sql
      .exec<{
        value: string;
      }>("SELECT value FROM fs_config WHERE key = 'chunk_size'")
      .toArray();

    let chunkSize: number;
    if (configRows.length === 0) {
      this.storage.sql.exec(
        "INSERT INTO fs_config (key, value) VALUES ('chunk_size', ?)",
        DEFAULT_CHUNK_SIZE.toString(),
      );
      chunkSize = DEFAULT_CHUNK_SIZE;
    } else {
      chunkSize = parseInt(configRows[0].value, 10) || DEFAULT_CHUNK_SIZE;
    }

    // Check compression mode - TypeScript SDK only supports 'none' for now
    const compressionRows = this.storage.sql
      .exec<{
        value: string;
      }>("SELECT value FROM fs_config WHERE key = 'compression'")
      .toArray();

    if (compressionRows.length > 0 && compressionRows[0].value !== "none") {
      throw new Error(
        `TypeScript SDK does not support compression mode '${compressionRows[0].value}'. ` +
          "Only 'none' is supported. Please use the Rust CLI or SDK to access " +
          "databases with compression enabled.",
      );
    }

    const rootRows = this.storage.sql
      .exec<{
        ino: number;
      }>("SELECT ino FROM fs_inode WHERE ino = ?", this.rootIno)
      .toArray();

    if (rootRows.length === 0) {
      const now = Math.floor(Date.now() / 1000);
      this.storage.sql.exec(
        `INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
         VALUES (?, ?, 1, 0, 0, 0, ?, ?, ?)`,
        this.rootIno,
        DEFAULT_DIR_MODE,
        now,
        now,
        now,
      );
    }

    return chunkSize;
  }

  private normalizePath(path: string): string {
    const normalized = path.replace(/\/+$/, "") || "/";
    return normalized.startsWith("/") ? normalized : "/" + normalized;
  }

  private splitPath(path: string): string[] {
    const normalized = this.normalizePath(path);
    if (normalized === "/") return [];
    return normalized.split("/").filter((p) => p);
  }

  private resolvePathOrThrow(
    path: string,
    syscall: string,
  ): { normalizedPath: string; ino: number } {
    const normalizedPath = this.normalizePath(path);
    if (normalizedPath === "/") {
      return { normalizedPath: "/", ino: this.rootIno };
    }

    const parts = this.splitPath(normalizedPath);
    let currentIno = this.rootIno;

    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];

      const rows = this.storage.sql
        .exec<{ ino: number; mode: number }>(
          `SELECT d.ino, i.mode
         FROM fs_dentry d
         JOIN fs_inode i ON d.ino = i.ino
         WHERE d.parent_ino = ? AND d.name = ?`,
          currentIno,
          part,
        )
        .toArray();

      if (rows.length === 0) {
        throw createFsError({
          code: "ENOENT",
          syscall,
          path: normalizedPath,
          message: "no such file or directory",
        });
      }

      currentIno = rows[0].ino;

      if (i < parts.length - 1 && (rows[0].mode & S_IFMT) !== S_IFDIR) {
        throw createFsError({
          code: "ENOTDIR",
          syscall,
          path: normalizedPath,
          message: "not a directory",
        });
      }
    }

    return { normalizedPath, ino: currentIno };
  }

  async stat(path: string): Promise<Stats> {
    const { ino } = this.resolvePathOrThrow(path, "stat");

    const rows = this.storage.sql
      .exec<{
        ino: number;
        mode: number;
        nlink: number;
        uid: number;
        gid: number;
        size: number;
        atime: number;
        mtime: number;
        ctime: number;
      }>(
        `SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime
       FROM fs_inode WHERE ino = ?`,
        ino,
      )
      .toArray();

    if (rows.length === 0) {
      throw createFsError({
        code: "ENOENT",
        syscall: "stat",
        path,
        message: "no such file or directory",
      });
    }

    return createStats(rows[0]);
  }

  async lstat(path: string): Promise<Stats> {
    return this.stat(path);
  }

  async open(path: string): Promise<FileHandle> {
    const { ino } = this.resolvePathOrThrow(path, "open");

    const rows = this.storage.sql
      .exec<{ mode: number }>("SELECT mode FROM fs_inode WHERE ino = ?", ino)
      .toArray();

    if (rows.length === 0 || (rows[0].mode & S_IFMT) !== S_IFREG) {
      throw createFsError({
        code: "EISDIR",
        syscall: "open",
        path,
        message: "illegal operation on a directory",
      });
    }

    return new AgentFSFile(this.storage, ino, this.chunkSize);
  }

  async readdir(path: string): Promise<DirEntry[]> {
    const { ino } = this.resolvePathOrThrow(path, "readdir");

    const modeRows = this.storage.sql
      .exec<{ mode: number }>("SELECT mode FROM fs_inode WHERE ino = ?", ino)
      .toArray();

    if (modeRows.length === 0 || (modeRows[0].mode & S_IFMT) !== S_IFDIR) {
      throw createFsError({
        code: "ENOTDIR",
        syscall: "readdir",
        path,
        message: "not a directory",
      });
    }

    const rows = this.storage.sql
      .exec<{ name: string; mode: number }>(
        `SELECT d.name, i.mode
       FROM fs_dentry d
       JOIN fs_inode i ON d.ino = i.ino
       WHERE d.parent_ino = ?
       ORDER BY d.name ASC`,
        ino,
      )
      .toArray();

    return rows.map((row) => ({ name: row.name, mode: row.mode }));
  }

  async mkdir(path: string, mode?: number): Promise<void> {
    const normalizedPath = this.normalizePath(path);
    const parts = this.splitPath(normalizedPath);

    if (parts.length === 0) {
      throw createFsError({
        code: "EEXIST",
        syscall: "mkdir",
        path,
        message: "file exists",
      });
    }

    const parentPath =
      parts.length === 1 ? "/" : "/" + parts.slice(0, -1).join("/");
    const { ino: parentIno } = this.resolvePathOrThrow(parentPath, "mkdir");
    const name = parts[parts.length - 1];

    const existingRows = this.storage.sql
      .exec<{
        ino: number;
      }>("SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?", parentIno, name)
      .toArray();

    if (existingRows.length > 0) {
      throw createFsError({
        code: "EEXIST",
        syscall: "mkdir",
        path,
        message: "file exists",
      });
    }

    const effectiveMode =
      mode !== undefined ? mode | S_IFDIR : DEFAULT_DIR_MODE;
    const now = Math.floor(Date.now() / 1000);

    this.storage.transactionSync(() => {
      this.storage.sql.exec(
        `INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime)
         VALUES (?, 1, 0, 0, 0, ?, ?, ?)`,
        effectiveMode,
        now,
        now,
        now,
      );

      const inoRows = this.storage.sql
        .exec<{ ino: number }>("SELECT last_insert_rowid() as ino")
        .toArray();
      const newIno = inoRows[0].ino;

      this.storage.sql.exec(
        `INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)`,
        name,
        parentIno,
        newIno,
      );
    });
  }

  async rmdir(path: string): Promise<void> {
    const { normalizedPath, ino } = this.resolvePathOrThrow(path, "rmdir");

    if (normalizedPath === "/") {
      throw createFsError({
        code: "EPERM",
        syscall: "rmdir",
        path,
        message: "operation not permitted",
      });
    }

    const modeRows = this.storage.sql
      .exec<{ mode: number }>("SELECT mode FROM fs_inode WHERE ino = ?", ino)
      .toArray();

    if (modeRows.length === 0 || (modeRows[0].mode & S_IFMT) !== S_IFDIR) {
      throw createFsError({
        code: "ENOTDIR",
        syscall: "rmdir",
        path,
        message: "not a directory",
      });
    }

    const childRows = this.storage.sql
      .exec<{
        count: number;
      }>("SELECT COUNT(*) as count FROM fs_dentry WHERE parent_ino = ?", ino)
      .toArray();

    if (childRows[0].count > 0) {
      throw createFsError({
        code: "ENOTEMPTY",
        syscall: "rmdir",
        path,
        message: "directory not empty",
      });
    }

    this.storage.transactionSync(() => {
      this.storage.sql.exec("DELETE FROM fs_dentry WHERE ino = ?", ino);
      this.storage.sql.exec("DELETE FROM fs_inode WHERE ino = ?", ino);
    });
  }

  async unlink(path: string): Promise<void> {
    const { ino } = this.resolvePathOrThrow(path, "unlink");

    const modeRows = this.storage.sql
      .exec<{
        mode: number;
        nlink: number;
      }>("SELECT mode, nlink FROM fs_inode WHERE ino = ?", ino)
      .toArray();

    if (modeRows.length === 0) {
      throw createFsError({
        code: "ENOENT",
        syscall: "unlink",
        path,
        message: "no such file or directory",
      });
    }

    if ((modeRows[0].mode & S_IFMT) === S_IFDIR) {
      throw createFsError({
        code: "EISDIR",
        syscall: "unlink",
        path,
        message: "is a directory",
      });
    }

    this.storage.transactionSync(() => {
      this.storage.sql.exec("DELETE FROM fs_dentry WHERE ino = ?", ino);

      const newNlink = modeRows[0].nlink - 1;
      if (newNlink <= 0) {
        this.storage.sql.exec("DELETE FROM fs_inode WHERE ino = ?", ino);
        this.storage.sql.exec("DELETE FROM fs_data WHERE ino = ?", ino);
        this.storage.sql.exec("DELETE FROM fs_symlink WHERE ino = ?", ino);
      } else {
        this.storage.sql.exec(
          "UPDATE fs_inode SET nlink = ? WHERE ino = ?",
          newNlink,
          ino,
        );
      }
    });
  }

  async rename(oldPath: string, newPath: string): Promise<void> {
    const { ino: oldIno } = this.resolvePathOrThrow(oldPath, "rename");

    const normalizedNewPath = this.normalizePath(newPath);
    const newParts = this.splitPath(normalizedNewPath);

    if (newParts.length === 0) {
      throw createFsError({
        code: "EINVAL",
        syscall: "rename",
        path: oldPath,
        message: "invalid argument",
      });
    }

    const newParentPath =
      newParts.length === 1 ? "/" : "/" + newParts.slice(0, -1).join("/");
    const { ino: newParentIno } = this.resolvePathOrThrow(
      newParentPath,
      "rename",
    );
    const newName = newParts[newParts.length - 1];

    this.storage.transactionSync(() => {
      const existingRows = this.storage.sql
        .exec<{ ino: number; mode: number }>(
          `SELECT d.ino, i.mode
         FROM fs_dentry d
         JOIN fs_inode i ON d.ino = i.ino
         WHERE d.parent_ino = ? AND d.name = ?`,
          newParentIno,
          newName,
        )
        .toArray();

      if (existingRows.length > 0) {
        const existingIno = existingRows[0].ino;
        if ((existingRows[0].mode & S_IFMT) === S_IFDIR) {
          const childRows = this.storage.sql
            .exec<{
              count: number;
            }>("SELECT COUNT(*) as count FROM fs_dentry WHERE parent_ino = ?", existingIno)
            .toArray();

          if (childRows[0].count > 0) {
            throw createFsError({
              code: "ENOTEMPTY",
              syscall: "rename",
              path: newPath,
              message: "directory not empty",
            });
          }
        }

        this.storage.sql.exec(
          "DELETE FROM fs_dentry WHERE ino = ?",
          existingIno,
        );
        this.storage.sql.exec(
          "DELETE FROM fs_inode WHERE ino = ?",
          existingIno,
        );
        this.storage.sql.exec("DELETE FROM fs_data WHERE ino = ?", existingIno);
        this.storage.sql.exec(
          "DELETE FROM fs_symlink WHERE ino = ?",
          existingIno,
        );
      }

      this.storage.sql.exec(
        "UPDATE fs_dentry SET parent_ino = ?, name = ? WHERE ino = ?",
        newParentIno,
        newName,
        oldIno,
      );
    });
  }

  async chmod(path: string, mode: number): Promise<void> {
    const { ino } = this.resolvePathOrThrow(path, "chmod");

    const modeRows = this.storage.sql
      .exec<{ mode: number }>("SELECT mode FROM fs_inode WHERE ino = ?", ino)
      .toArray();

    if (modeRows.length === 0) {
      throw createFsError({
        code: "ENOENT",
        syscall: "chmod",
        path,
        message: "no such file or directory",
      });
    }

    const newMode = (modeRows[0].mode & S_IFMT) | (mode & 0o7777);
    this.storage.sql.exec(
      "UPDATE fs_inode SET mode = ? WHERE ino = ?",
      newMode,
      ino,
    );
  }

  async writeFile(path: string, data: Buffer): Promise<void> {
    const normalizedPath = this.normalizePath(path);
    const parts = this.splitPath(normalizedPath);

    if (parts.length === 0) {
      throw createFsError({
        code: "EISDIR",
        syscall: "writeFile",
        path,
        message: "is a directory",
      });
    }

    const parentPath =
      parts.length === 1 ? "/" : "/" + parts.slice(0, -1).join("/");
    const { ino: parentIno } = this.resolvePathOrThrow(parentPath, "writeFile");
    const name = parts[parts.length - 1];

    this.storage.transactionSync(() => {
      const existingRows = this.storage.sql
        .exec<{ ino: number; mode: number }>(
          `SELECT d.ino, i.mode
         FROM fs_dentry d
         JOIN fs_inode i ON d.ino = i.ino
         WHERE d.parent_ino = ? AND d.name = ?`,
          parentIno,
          name,
        )
        .toArray();

      let ino: number;

      if (existingRows.length > 0) {
        ino = existingRows[0].ino;
        if ((existingRows[0].mode & S_IFMT) !== S_IFREG) {
          throw createFsError({
            code: "EISDIR",
            syscall: "writeFile",
            path,
            message: "is a directory",
          });
        }

        this.storage.sql.exec("DELETE FROM fs_data WHERE ino = ?", ino);
      } else {
        const now = Math.floor(Date.now() / 1000);
        this.storage.sql.exec(
          `INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime)
           VALUES (?, 1, 0, 0, 0, ?, ?, ?)`,
          DEFAULT_FILE_MODE,
          now,
          now,
          now,
        );

        const inoRows = this.storage.sql
          .exec<{ ino: number }>("SELECT last_insert_rowid() as ino")
          .toArray();
        ino = inoRows[0].ino;

        this.storage.sql.exec(
          `INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)`,
          name,
          parentIno,
          ino,
        );
      }

      const now = Math.floor(Date.now() / 1000);
      this.storage.sql.exec(
        "UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?",
        data.length,
        now,
        ino,
      );

      let chunkIndex = 0;
      for (let offset = 0; offset < data.length; offset += this.chunkSize) {
        const chunk = data.subarray(
          offset,
          Math.min(offset + this.chunkSize, data.length),
        );
        this.storage.sql.exec(
          `INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)`,
          ino,
          chunkIndex,
          chunk,
        );
        chunkIndex++;
      }
    });
  }

  async readFile(path: string): Promise<Buffer> {
    const { ino } = this.resolvePathOrThrow(path, "readFile");

    const modeRows = this.storage.sql
      .exec<{
        mode: number;
        size: number;
      }>("SELECT mode, size FROM fs_inode WHERE ino = ?", ino)
      .toArray();

    if (modeRows.length === 0) {
      throw createFsError({
        code: "ENOENT",
        syscall: "readFile",
        path,
        message: "no such file or directory",
      });
    }

    if ((modeRows[0].mode & S_IFMT) !== S_IFREG) {
      throw createFsError({
        code: "EISDIR",
        syscall: "readFile",
        path,
        message: "is a directory",
      });
    }

    const size = modeRows[0].size;
    if (size === 0) {
      return Buffer.alloc(0);
    }

    const dataRows = this.storage.sql
      .exec<{
        data: ArrayBuffer;
      }>(`SELECT data FROM fs_data WHERE ino = ? ORDER BY chunk_index ASC`, ino)
      .toArray();

    return Buffer.concat(dataRows.map((row) => Buffer.from(row.data)));
  }

  async symlink(target: string, path: string): Promise<void> {
    const normalizedPath = this.normalizePath(path);
    const parts = this.splitPath(normalizedPath);

    if (parts.length === 0) {
      throw createFsError({
        code: "EEXIST",
        syscall: "symlink",
        path,
        message: "file exists",
      });
    }

    const parentPath =
      parts.length === 1 ? "/" : "/" + parts.slice(0, -1).join("/");
    const { ino: parentIno } = this.resolvePathOrThrow(parentPath, "symlink");
    const name = parts[parts.length - 1];

    const existingRows = this.storage.sql
      .exec<{
        ino: number;
      }>("SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?", parentIno, name)
      .toArray();

    if (existingRows.length > 0) {
      throw createFsError({
        code: "EEXIST",
        syscall: "symlink",
        path,
        message: "file exists",
      });
    }

    const now = Math.floor(Date.now() / 1000);

    this.storage.transactionSync(() => {
      this.storage.sql.exec(
        `INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime)
         VALUES (?, 1, 0, 0, 0, ?, ?, ?)`,
        S_IFLNK | 0o777,
        now,
        now,
        now,
      );

      const inoRows = this.storage.sql
        .exec<{ ino: number }>("SELECT last_insert_rowid() as ino")
        .toArray();
      const newIno = inoRows[0].ino;

      this.storage.sql.exec(
        `INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)`,
        name,
        parentIno,
        newIno,
      );

      this.storage.sql.exec(
        `INSERT INTO fs_symlink (ino, target) VALUES (?, ?)`,
        newIno,
        target,
      );
    });
  }

  async readlink(path: string): Promise<string> {
    const { ino } = this.resolvePathOrThrow(path, "readlink");

    const rows = this.storage.sql
      .exec<{
        target: string;
      }>("SELECT target FROM fs_symlink WHERE ino = ?", ino)
      .toArray();

    if (rows.length === 0) {
      throw createFsError({
        code: "EINVAL",
        syscall: "readlink",
        path,
        message: "invalid argument",
      });
    }

    return rows[0].target;
  }

  async statfs(): Promise<FilesystemStats> {
    const inoRows = this.storage.sql
      .exec<{ count: number }>("SELECT COUNT(*) as count FROM fs_inode")
      .toArray();

    return {
      files: inoRows[0].count,
      filesAvail: 1000000 - inoRows[0].count,
    };
  }
}
