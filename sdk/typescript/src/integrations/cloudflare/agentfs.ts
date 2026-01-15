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
} from '../../filesystem/interface.js';

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
  exec<T = Record<string, unknown>>(query: string, ...bindings: unknown[]): SqlStorageCursor<T>;
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
type FsErrorCode = 'ENOENT' | 'EEXIST' | 'EISDIR' | 'ENOTDIR' | 'ENOTEMPTY' | 'EPERM' | 'EINVAL';

interface FsError extends Error {
  code: FsErrorCode;
  syscall: string;
  path: string;
}

function createFsError(opts: { code: FsErrorCode; syscall: string; path: string; message: string }): FsError {
  const err = new Error(`${opts.code}: ${opts.message}, ${opts.syscall} '${opts.path}'`) as FsError;
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

    const rows = this.storage.sql.exec<{ chunk_index: number; data: ArrayBuffer }>(
      `SELECT chunk_index, data FROM fs_data
       WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ?
       ORDER BY chunk_index ASC`,
      this.ino, startChunk, endChunk
    ).toArray();

    const buffers: Buffer[] = [];
    let bytesCollected = 0;
    const startOffsetInChunk = offset % this.chunkSize;

    for (const row of rows) {
      const data = Buffer.from(row.data);
      const skip = buffers.length === 0 ? startOffsetInChunk : 0;
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

    this.storage.transactionSync(() => {
      const sizeRow = this.storage.sql.exec<{ size: number }>(
        'SELECT size FROM fs_inode WHERE ino = ?',
        this.ino
      ).toArray()[0];
      const currentSize = sizeRow?.size ?? 0;

      if (offset > currentSize) {
        const zeros = Buffer.alloc(offset - currentSize);
        this.writeDataAtOffset(currentSize, zeros);
      }

      this.writeDataAtOffset(offset, data);

      const newSize = Math.max(currentSize, offset + data.length);
      const now = Math.floor(Date.now() / 1000);
      this.storage.sql.exec(
        'UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?',
        newSize, now, this.ino
      );
    });
  }

  private writeDataAtOffset(offset: number, data: Buffer): void {
    const startChunk = Math.floor(offset / this.chunkSize);
    const endChunk = Math.floor((offset + data.length - 1) / this.chunkSize);

    for (let chunkIdx = startChunk; chunkIdx <= endChunk; chunkIdx++) {
      const chunkStart = chunkIdx * this.chunkSize;
      const chunkEnd = chunkStart + this.chunkSize;

      const dataStart = Math.max(0, chunkStart - offset);
      const dataEnd = Math.min(data.length, chunkEnd - offset);
      const writeOffset = Math.max(0, offset - chunkStart);

      const existingRows = this.storage.sql.exec<{ data: ArrayBuffer }>(
        'SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?',
        this.ino, chunkIdx
      ).toArray();

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
        `INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)
         ON CONFLICT(ino, chunk_index) DO UPDATE SET data = excluded.data`,
        this.ino, chunkIdx, chunkData
      );
    }
  }

  async truncate(newSize: number): Promise<void> {
    this.storage.transactionSync(() => {
      const sizeRow = this.storage.sql.exec<{ size: number }>(
        'SELECT size FROM fs_inode WHERE ino = ?',
        this.ino
      ).toArray()[0];
      const currentSize = sizeRow?.size ?? 0;

      if (newSize === 0) {
        this.storage.sql.exec('DELETE FROM fs_data WHERE ino = ?', this.ino);
      } else if (newSize < currentSize) {
        const lastChunkIdx = Math.floor((newSize - 1) / this.chunkSize);

        this.storage.sql.exec(
          'DELETE FROM fs_data WHERE ino = ? AND chunk_index > ?',
          this.ino, lastChunkIdx
        );

        const offsetInChunk = newSize % this.chunkSize;
        if (offsetInChunk > 0) {
          const rows = this.storage.sql.exec<{ data: ArrayBuffer }>(
            'SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?',
            this.ino, lastChunkIdx
          ).toArray();

          if (rows.length > 0) {
            const existingData = Buffer.from(rows[0].data);
            if (existingData.length > offsetInChunk) {
              const truncatedChunk = existingData.subarray(0, offsetInChunk);
              this.storage.sql.exec(
                'UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?',
                truncatedChunk, this.ino, lastChunkIdx
              );
            }
          }
        }
      }

      const now = Math.floor(Date.now() / 1000);
      this.storage.sql.exec(
        'UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?',
        newSize, now, this.ino
      );
    });
  }

  async fsync(): Promise<void> {
    // Cloudflare Durable Objects automatically persist data
    // No explicit sync needed
  }

  async fstat(): Promise<Stats> {
    const rows = this.storage.sql.exec<{
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
      this.ino
    ).toArray();

    if (rows.length === 0) {
      throw new Error('File handle refers to deleted inode');
    }

    return createStats(rows[0]);
  }
}

/**
 * A filesystem backed by Cloudflare Durable Objects SQLite storage.
 *
 * AgentFS implements the FileSystem interface using Cloudflare's
 * Durable Objects SQLite storage as the backing store.
 *
 * @example
 * ```typescript
 * // In a Durable Object class
 * export class MyDurableObject extends DurableObject {
 *   private fs: AgentFS;
 *
 *   constructor(ctx: DurableObjectState, env: Env) {
 *     super(ctx, env);
 *     this.fs = AgentFS.create(ctx.storage);
 *   }
 *
 *   async fetch(request: Request) {
 *     await this.fs.writeFile('/hello.txt', 'Hello, World!');
 *     const content = await this.fs.readFile('/hello.txt', 'utf8');
 *     return new Response(content);
 *   }
 * }
 * ```
 */
export class AgentFS implements FileSystem {
  private storage: CloudflareStorage;
  private rootIno: number = 1;
  private chunkSize: number = DEFAULT_CHUNK_SIZE;

  private constructor(storage: CloudflareStorage) {
    this.storage = storage;
  }

  /**
   * Create a AgentFS from a Durable Object storage context.
   *
   * @param storage - The ctx.storage from a Durable Object
   */
  static create(storage: CloudflareStorage): AgentFS {
    const fs = new AgentFS(storage);
    fs.initialize();
    return fs;
  }

  getChunkSize(): number {
    return this.chunkSize;
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
        ino INTEGER PRIMARY KEY AUTOINCREMENT,
        mode INTEGER NOT NULL,
        nlink INTEGER NOT NULL DEFAULT 0,
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
        name TEXT NOT NULL,
        parent_ino INTEGER NOT NULL,
        ino INTEGER NOT NULL,
        PRIMARY KEY(parent_ino, name)
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

    this.chunkSize = this.ensureRoot();
  }

  private ensureRoot(): number {
    const configRows = this.storage.sql.exec<{ value: string }>(
      "SELECT value FROM fs_config WHERE key = 'chunk_size'"
    ).toArray();

    let chunkSize: number;
    if (configRows.length === 0) {
      this.storage.sql.exec(
        "INSERT INTO fs_config (key, value) VALUES ('chunk_size', ?)",
        DEFAULT_CHUNK_SIZE.toString()
      );
      chunkSize = DEFAULT_CHUNK_SIZE;
    } else {
      chunkSize = parseInt(configRows[0].value, 10) || DEFAULT_CHUNK_SIZE;
    }

    const rootRows = this.storage.sql.exec<{ ino: number }>(
      'SELECT ino FROM fs_inode WHERE ino = ?',
      this.rootIno
    ).toArray();

    if (rootRows.length === 0) {
      const now = Math.floor(Date.now() / 1000);
      this.storage.sql.exec(
        `INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
         VALUES (?, ?, 1, 0, 0, 0, ?, ?, ?)`,
        this.rootIno, DEFAULT_DIR_MODE, now, now, now
      );
    }

    return chunkSize;
  }

  private normalizePath(path: string): string {
    const normalized = path.replace(/\/+$/, '') || '/';
    return normalized.startsWith('/') ? normalized : '/' + normalized;
  }

  private splitPath(path: string): string[] {
    const normalized = this.normalizePath(path);
    if (normalized === '/') return [];
    return normalized.split('/').filter(p => p);
  }

  private resolvePath(path: string): number | null {
    const normalized = this.normalizePath(path);

    if (normalized === '/') {
      return this.rootIno;
    }

    const parts = this.splitPath(normalized);
    let currentIno = this.rootIno;

    for (const name of parts) {
      const rows = this.storage.sql.exec<{ ino: number }>(
        'SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?',
        currentIno, name
      ).toArray();

      if (rows.length === 0) {
        return null;
      }

      currentIno = rows[0].ino;
    }

    return currentIno;
  }

  private resolvePathOrThrow(path: string, syscall: string): { normalizedPath: string; ino: number } {
    const normalizedPath = this.normalizePath(path);
    const ino = this.resolvePath(normalizedPath);
    if (ino === null) {
      throw createFsError({
        code: 'ENOENT',
        syscall,
        path: normalizedPath,
        message: 'no such file or directory',
      });
    }
    return { normalizedPath, ino };
  }

  private resolveParent(path: string): { parentIno: number; name: string } | null {
    const normalized = this.normalizePath(path);

    if (normalized === '/') {
      return null;
    }

    const parts = this.splitPath(normalized);
    const name = parts[parts.length - 1];
    const parentPath = parts.length === 1 ? '/' : '/' + parts.slice(0, -1).join('/');

    const parentIno = this.resolvePath(parentPath);

    if (parentIno === null) {
      return null;
    }

    return { parentIno, name };
  }

  private createInode(mode: number, uid: number = 0, gid: number = 0): number {
    const now = Math.floor(Date.now() / 1000);
    this.storage.sql.exec(
      `INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
       VALUES (?, ?, ?, 0, ?, ?, ?)`,
      mode, uid, gid, now, now, now
    );

    // Get the last inserted rowid
    const rows = this.storage.sql.exec<{ ino: number }>(
      'SELECT last_insert_rowid() as ino'
    ).toArray();

    return rows[0].ino;
  }

  private createDentry(parentIno: number, name: string, ino: number): void {
    this.storage.sql.exec(
      'INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)',
      name, parentIno, ino
    );

    this.storage.sql.exec(
      'UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?',
      ino
    );
  }

  private ensureParentDirs(path: string): void {
    const parts = this.splitPath(path);
    parts.pop();

    let currentIno = this.rootIno;

    for (const name of parts) {
      const rows = this.storage.sql.exec<{ ino: number }>(
        'SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?',
        currentIno, name
      ).toArray();

      if (rows.length === 0) {
        const dirIno = this.createInode(DEFAULT_DIR_MODE);
        this.createDentry(currentIno, name, dirIno);
        currentIno = dirIno;
      } else {
        const mode = this.getInodeMode(rows[0].ino);
        if (mode !== null && (mode & S_IFMT) !== S_IFDIR) {
          throw createFsError({
            code: 'ENOTDIR',
            syscall: 'open',
            path: this.normalizePath(path),
            message: 'not a directory',
          });
        }
        currentIno = rows[0].ino;
      }
    }
  }

  private getLinkCount(ino: number): number {
    const rows = this.storage.sql.exec<{ nlink: number }>(
      'SELECT nlink FROM fs_inode WHERE ino = ?',
      ino
    ).toArray();
    return rows[0]?.nlink ?? 0;
  }

  private getInodeMode(ino: number): number | null {
    const rows = this.storage.sql.exec<{ mode: number }>(
      'SELECT mode FROM fs_inode WHERE ino = ?',
      ino
    ).toArray();
    return rows[0]?.mode ?? null;
  }

  // ==================== FileSystem Interface Implementation ====================

  async writeFile(
    path: string,
    content: string | Buffer,
    options?: BufferEncoding | { encoding?: BufferEncoding }
  ): Promise<void> {
    const encoding = typeof options === 'string'
      ? options
      : options?.encoding;

    const buffer = typeof content === 'string'
      ? Buffer.from(content, encoding ?? 'utf8')
      : content;

    this.storage.transactionSync(() => {
      this.ensureParentDirs(path);

      const ino = this.resolvePath(path);
      const normalizedPath = this.normalizePath(path);

      if (ino !== null) {
        const mode = this.getInodeMode(ino);
        if (mode !== null && (mode & S_IFMT) === S_IFDIR) {
          throw createFsError({
            code: 'EISDIR',
            syscall: 'open',
            path: normalizedPath,
            message: 'illegal operation on a directory',
          });
        }
        this.updateFileContent(ino, buffer);
      } else {
        const parent = this.resolveParent(path);
        if (!parent) {
          throw createFsError({
            code: 'ENOENT',
            syscall: 'open',
            path: normalizedPath,
            message: 'no such file or directory',
          });
        }

        const fileIno = this.createInode(DEFAULT_FILE_MODE);
        this.createDentry(parent.parentIno, parent.name, fileIno);
        this.updateFileContent(fileIno, buffer);
      }
    });
  }

  private updateFileContent(ino: number, buffer: Buffer): void {
    const now = Math.floor(Date.now() / 1000);

    this.storage.sql.exec('DELETE FROM fs_data WHERE ino = ?', ino);

    if (buffer.length > 0) {
      let chunkIndex = 0;
      for (let offset = 0; offset < buffer.length; offset += this.chunkSize) {
        const chunk = buffer.subarray(offset, Math.min(offset + this.chunkSize, buffer.length));
        this.storage.sql.exec(
          'INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)',
          ino, chunkIndex, chunk
        );
        chunkIndex++;
      }
    }

    this.storage.sql.exec(
      'UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?',
      buffer.length, now, ino
    );
  }

  async readFile(path: string): Promise<Buffer>;
  async readFile(path: string, encoding: BufferEncoding): Promise<string>;
  async readFile(path: string, options: { encoding: BufferEncoding }): Promise<string>;
  async readFile(
    path: string,
    options?: BufferEncoding | { encoding?: BufferEncoding }
  ): Promise<Buffer | string> {
    const encoding = typeof options === 'string'
      ? options
      : options?.encoding;

    const { normalizedPath, ino } = this.resolvePathOrThrow(path, 'open');

    const mode = this.getInodeMode(ino);
    if (mode !== null && (mode & S_IFMT) === S_IFDIR) {
      throw createFsError({
        code: 'EISDIR',
        syscall: 'open',
        path: normalizedPath,
        message: 'illegal operation on a directory',
      });
    }

    const rows = this.storage.sql.exec<{ data: ArrayBuffer }>(
      'SELECT data FROM fs_data WHERE ino = ? ORDER BY chunk_index ASC',
      ino
    ).toArray();

    let combined: Buffer;
    if (rows.length === 0) {
      combined = Buffer.alloc(0);
    } else {
      const buffers = rows.map(row => Buffer.from(row.data));
      combined = Buffer.concat(buffers);
    }

    const now = Math.floor(Date.now() / 1000);
    this.storage.sql.exec('UPDATE fs_inode SET atime = ? WHERE ino = ?', now, ino);

    if (encoding) {
      return combined.toString(encoding);
    }
    return combined;
  }

  async readdir(path: string): Promise<string[]> {
    const { normalizedPath, ino } = this.resolvePathOrThrow(path, 'scandir');

    const mode = this.getInodeMode(ino);
    if (mode !== null && (mode & S_IFMT) !== S_IFDIR) {
      throw createFsError({
        code: 'ENOTDIR',
        syscall: 'scandir',
        path: normalizedPath,
        message: 'not a directory',
      });
    }

    const rows = this.storage.sql.exec<{ name: string }>(
      'SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name ASC',
      ino
    ).toArray();

    return rows.map(row => row.name);
  }

  async readdirPlus(path: string): Promise<DirEntry[]> {
    const { normalizedPath, ino } = this.resolvePathOrThrow(path, 'scandir');

    const mode = this.getInodeMode(ino);
    if (mode !== null && (mode & S_IFMT) !== S_IFDIR) {
      throw createFsError({
        code: 'ENOTDIR',
        syscall: 'scandir',
        path: normalizedPath,
        message: 'not a directory',
      });
    }

    const rows = this.storage.sql.exec<{
      name: string;
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
      `SELECT d.name, i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime
       FROM fs_dentry d
       JOIN fs_inode i ON d.ino = i.ino
       WHERE d.parent_ino = ?
       ORDER BY d.name ASC`,
      ino
    ).toArray();

    return rows.map(row => ({
      name: row.name,
      stats: createStats({
        ino: row.ino,
        mode: row.mode,
        nlink: row.nlink,
        uid: row.uid,
        gid: row.gid,
        size: row.size,
        atime: row.atime,
        mtime: row.mtime,
        ctime: row.ctime,
      }),
    }));
  }

  async stat(path: string): Promise<Stats> {
    const { normalizedPath, ino } = this.resolvePathOrThrow(path, 'stat');

    const rows = this.storage.sql.exec<{
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
      ino
    ).toArray();

    if (rows.length === 0) {
      throw createFsError({
        code: 'ENOENT',
        syscall: 'stat',
        path: normalizedPath,
        message: 'no such file or directory',
      });
    }

    return createStats(rows[0]);
  }

  async lstat(path: string): Promise<Stats> {
    return this.stat(path);
  }

  async mkdir(path: string): Promise<void> {
    const normalizedPath = this.normalizePath(path);

    const existing = this.resolvePath(normalizedPath);
    if (existing !== null) {
      throw createFsError({
        code: 'EEXIST',
        syscall: 'mkdir',
        path: normalizedPath,
        message: 'file already exists',
      });
    }

    const parent = this.resolveParent(normalizedPath);
    if (!parent) {
      throw createFsError({
        code: 'ENOENT',
        syscall: 'mkdir',
        path: normalizedPath,
        message: 'no such file or directory',
      });
    }

    const parentMode = this.getInodeMode(parent.parentIno);
    if (parentMode !== null && (parentMode & S_IFMT) !== S_IFDIR) {
      throw createFsError({
        code: 'ENOTDIR',
        syscall: 'mkdir',
        path: normalizedPath,
        message: 'not a directory',
      });
    }

    this.storage.transactionSync(() => {
      const dirIno = this.createInode(DEFAULT_DIR_MODE);
      try {
        this.createDentry(parent.parentIno, parent.name, dirIno);
      } catch {
        throw createFsError({
          code: 'EEXIST',
          syscall: 'mkdir',
          path: normalizedPath,
          message: 'file already exists',
        });
      }
    });
  }

  async rmdir(path: string): Promise<void> {
    const normalizedPath = this.normalizePath(path);

    if (normalizedPath === '/') {
      throw createFsError({
        code: 'EPERM',
        syscall: 'rmdir',
        path: normalizedPath,
        message: 'operation not permitted',
      });
    }

    const { ino } = this.resolvePathOrThrow(normalizedPath, 'rmdir');

    const mode = this.getInodeMode(ino);
    if (mode === null || (mode & S_IFMT) !== S_IFDIR) {
      throw createFsError({
        code: 'ENOTDIR',
        syscall: 'rmdir',
        path: normalizedPath,
        message: 'not a directory',
      });
    }

    const children = this.storage.sql.exec<{ one: number }>(
      'SELECT 1 as one FROM fs_dentry WHERE parent_ino = ? LIMIT 1',
      ino
    ).toArray();

    if (children.length > 0) {
      throw createFsError({
        code: 'ENOTEMPTY',
        syscall: 'rmdir',
        path: normalizedPath,
        message: 'directory not empty',
      });
    }

    const parent = this.resolveParent(normalizedPath);
    if (!parent) {
      throw createFsError({
        code: 'EPERM',
        syscall: 'rmdir',
        path: normalizedPath,
        message: 'operation not permitted',
      });
    }

    this.removeDentryAndMaybeInode(parent.parentIno, parent.name, ino);
  }

  async unlink(path: string): Promise<void> {
    const normalizedPath = this.normalizePath(path);

    if (normalizedPath === '/') {
      throw createFsError({
        code: 'EPERM',
        syscall: 'unlink',
        path: normalizedPath,
        message: 'operation not permitted',
      });
    }

    const { ino } = this.resolvePathOrThrow(normalizedPath, 'unlink');

    const mode = this.getInodeMode(ino);
    if (mode !== null && (mode & S_IFMT) === S_IFDIR) {
      throw createFsError({
        code: 'EISDIR',
        syscall: 'unlink',
        path: normalizedPath,
        message: 'illegal operation on a directory',
      });
    }

    const parent = this.resolveParent(normalizedPath)!;

    this.storage.transactionSync(() => {
      this.storage.sql.exec(
        'DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?',
        parent.parentIno, parent.name
      );

      this.storage.sql.exec(
        'UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?',
        ino
      );

      const linkCount = this.getLinkCount(ino);
      if (linkCount === 0) {
        this.storage.sql.exec('DELETE FROM fs_inode WHERE ino = ?', ino);
        this.storage.sql.exec('DELETE FROM fs_data WHERE ino = ?', ino);
      }
    });
  }

  async rm(
    path: string,
    options?: { force?: boolean; recursive?: boolean }
  ): Promise<void> {
    const normalizedPath = this.normalizePath(path);
    const force = options?.force ?? false;
    const recursive = options?.recursive ?? false;

    if (normalizedPath === '/') {
      throw createFsError({
        code: 'EPERM',
        syscall: 'rm',
        path: normalizedPath,
        message: 'operation not permitted',
      });
    }

    const ino = this.resolvePath(normalizedPath);
    if (ino === null) {
      if (!force) {
        throw createFsError({
          code: 'ENOENT',
          syscall: 'rm',
          path: normalizedPath,
          message: 'no such file or directory',
        });
      }
      return;
    }

    const mode = this.getInodeMode(ino);
    if (mode === null) {
      if (!force) {
        throw createFsError({
          code: 'ENOENT',
          syscall: 'rm',
          path: normalizedPath,
          message: 'no such file or directory',
        });
      }
      return;
    }

    const parent = this.resolveParent(normalizedPath);
    if (!parent) {
      throw createFsError({
        code: 'EPERM',
        syscall: 'rm',
        path: normalizedPath,
        message: 'operation not permitted',
      });
    }

    if ((mode & S_IFMT) === S_IFDIR) {
      if (!recursive) {
        throw createFsError({
          code: 'EISDIR',
          syscall: 'rm',
          path: normalizedPath,
          message: 'illegal operation on a directory',
        });
      }

      this.storage.transactionSync(() => {
        this.rmDirContentsRecursive(ino);
        this.removeDentryAndMaybeInode(parent.parentIno, parent.name, ino);
      });
      return;
    }

    this.removeDentryAndMaybeInode(parent.parentIno, parent.name, ino);
  }

  private rmDirContentsRecursive(dirIno: number): void {
    const children = this.storage.sql.exec<{ name: string; ino: number }>(
      'SELECT name, ino FROM fs_dentry WHERE parent_ino = ? ORDER BY name ASC',
      dirIno
    ).toArray();

    for (const child of children) {
      const mode = this.getInodeMode(child.ino);
      if (mode === null) {
        continue;
      }

      if ((mode & S_IFMT) === S_IFDIR) {
        this.rmDirContentsRecursive(child.ino);
        this.removeDentryAndMaybeInode(dirIno, child.name, child.ino);
      } else {
        this.removeDentryAndMaybeInode(dirIno, child.name, child.ino);
      }
    }
  }

  private removeDentryAndMaybeInode(parentIno: number, name: string, ino: number): void {
    this.storage.sql.exec(
      'DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?',
      parentIno, name
    );

    this.storage.sql.exec(
      'UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?',
      ino
    );

    const linkCount = this.getLinkCount(ino);
    if (linkCount === 0) {
      this.storage.sql.exec('DELETE FROM fs_inode WHERE ino = ?', ino);
      this.storage.sql.exec('DELETE FROM fs_data WHERE ino = ?', ino);
      this.storage.sql.exec('DELETE FROM fs_symlink WHERE ino = ?', ino);
    }
  }

  async rename(oldPath: string, newPath: string): Promise<void> {
    const oldNormalized = this.normalizePath(oldPath);
    const newNormalized = this.normalizePath(newPath);

    if (oldNormalized === newNormalized) return;

    if (oldNormalized === '/' || newNormalized === '/') {
      throw createFsError({
        code: 'EPERM',
        syscall: 'rename',
        path: oldNormalized === '/' ? oldNormalized : newNormalized,
        message: 'operation not permitted',
      });
    }

    const oldParent = this.resolveParent(oldNormalized);
    if (!oldParent) {
      throw createFsError({
        code: 'EPERM',
        syscall: 'rename',
        path: oldNormalized,
        message: 'operation not permitted',
      });
    }

    const newParent = this.resolveParent(newNormalized);
    if (!newParent) {
      throw createFsError({
        code: 'ENOENT',
        syscall: 'rename',
        path: newNormalized,
        message: 'no such file or directory',
      });
    }

    this.storage.transactionSync(() => {
      const { ino: oldIno } = this.resolvePathOrThrow(oldNormalized, 'rename');
      const oldMode = this.getInodeMode(oldIno);
      if (oldMode === null) {
        throw createFsError({
          code: 'ENOENT',
          syscall: 'rename',
          path: oldNormalized,
          message: 'no such file or directory',
        });
      }
      const oldIsDir = (oldMode & S_IFMT) === S_IFDIR;

      if (oldIsDir && newNormalized.startsWith(oldNormalized + '/')) {
        throw createFsError({
          code: 'EINVAL',
          syscall: 'rename',
          path: newNormalized,
          message: 'invalid argument',
        });
      }

      const newIno = this.resolvePath(newNormalized);
      if (newIno !== null) {
        const newMode = this.getInodeMode(newIno);
        if (newMode !== null) {
          const newIsDir = (newMode & S_IFMT) === S_IFDIR;

          if (newIsDir && !oldIsDir) {
            throw createFsError({
              code: 'EISDIR',
              syscall: 'rename',
              path: newNormalized,
              message: 'illegal operation on a directory',
            });
          }
          if (!newIsDir && oldIsDir) {
            throw createFsError({
              code: 'ENOTDIR',
              syscall: 'rename',
              path: newNormalized,
              message: 'not a directory',
            });
          }

          if (newIsDir) {
            const children = this.storage.sql.exec<{ one: number }>(
              'SELECT 1 as one FROM fs_dentry WHERE parent_ino = ? LIMIT 1',
              newIno
            ).toArray();
            if (children.length > 0) {
              throw createFsError({
                code: 'ENOTEMPTY',
                syscall: 'rename',
                path: newNormalized,
                message: 'directory not empty',
              });
            }
          }

          this.removeDentryAndMaybeInode(newParent.parentIno, newParent.name, newIno);
        }
      }

      this.storage.sql.exec(
        'UPDATE fs_dentry SET parent_ino = ?, name = ? WHERE parent_ino = ? AND name = ?',
        newParent.parentIno, newParent.name, oldParent.parentIno, oldParent.name
      );

      const now = Math.floor(Date.now() / 1000);
      this.storage.sql.exec('UPDATE fs_inode SET ctime = ? WHERE ino = ?', now, oldIno);
      this.storage.sql.exec('UPDATE fs_inode SET mtime = ?, ctime = ? WHERE ino = ?', now, now, oldParent.parentIno);
      if (newParent.parentIno !== oldParent.parentIno) {
        this.storage.sql.exec('UPDATE fs_inode SET mtime = ?, ctime = ? WHERE ino = ?', now, now, newParent.parentIno);
      }
    });
  }

  async copyFile(src: string, dest: string): Promise<void> {
    const srcNormalized = this.normalizePath(src);
    const destNormalized = this.normalizePath(dest);

    if (srcNormalized === destNormalized) {
      throw createFsError({
        code: 'EINVAL',
        syscall: 'copyfile',
        path: destNormalized,
        message: 'invalid argument',
      });
    }

    const { ino: srcIno } = this.resolvePathOrThrow(srcNormalized, 'copyfile');

    const srcMode = this.getInodeMode(srcIno);
    if (srcMode !== null && (srcMode & S_IFMT) === S_IFDIR) {
      throw createFsError({
        code: 'EISDIR',
        syscall: 'copyfile',
        path: srcNormalized,
        message: 'illegal operation on a directory',
      });
    }

    const srcRows = this.storage.sql.exec<{ mode: number; uid: number; gid: number; size: number }>(
      'SELECT mode, uid, gid, size FROM fs_inode WHERE ino = ?',
      srcIno
    ).toArray();

    if (srcRows.length === 0) {
      throw createFsError({
        code: 'ENOENT',
        syscall: 'copyfile',
        path: srcNormalized,
        message: 'no such file or directory',
      });
    }

    const srcRow = srcRows[0];

    const destParent = this.resolveParent(destNormalized);
    if (!destParent) {
      throw createFsError({
        code: 'ENOENT',
        syscall: 'copyfile',
        path: destNormalized,
        message: 'no such file or directory',
      });
    }

    this.storage.transactionSync(() => {
      const now = Math.floor(Date.now() / 1000);

      const destIno = this.resolvePath(destNormalized);
      if (destIno !== null) {
        const destMode = this.getInodeMode(destIno);
        if (destMode !== null && (destMode & S_IFMT) === S_IFDIR) {
          throw createFsError({
            code: 'EISDIR',
            syscall: 'copyfile',
            path: destNormalized,
            message: 'illegal operation on a directory',
          });
        }

        this.storage.sql.exec('DELETE FROM fs_data WHERE ino = ?', destIno);

        // Copy data chunks
        const srcData = this.storage.sql.exec<{ chunk_index: number; data: ArrayBuffer }>(
          'SELECT chunk_index, data FROM fs_data WHERE ino = ? ORDER BY chunk_index ASC',
          srcIno
        ).toArray();

        for (const chunk of srcData) {
          this.storage.sql.exec(
            'INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)',
            destIno, chunk.chunk_index, Buffer.from(chunk.data)
          );
        }

        this.storage.sql.exec(
          'UPDATE fs_inode SET mode = ?, uid = ?, gid = ?, size = ?, mtime = ?, ctime = ? WHERE ino = ?',
          srcRow.mode, srcRow.uid, srcRow.gid, srcRow.size, now, now, destIno
        );
      } else {
        const destInoCreated = this.createInode(srcRow.mode, srcRow.uid, srcRow.gid);
        this.createDentry(destParent.parentIno, destParent.name, destInoCreated);

        // Copy data chunks
        const srcData = this.storage.sql.exec<{ chunk_index: number; data: ArrayBuffer }>(
          'SELECT chunk_index, data FROM fs_data WHERE ino = ? ORDER BY chunk_index ASC',
          srcIno
        ).toArray();

        for (const chunk of srcData) {
          this.storage.sql.exec(
            'INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)',
            destInoCreated, chunk.chunk_index, Buffer.from(chunk.data)
          );
        }

        this.storage.sql.exec(
          'UPDATE fs_inode SET size = ?, mtime = ?, ctime = ? WHERE ino = ?',
          srcRow.size, now, now, destInoCreated
        );
      }
    });
  }

  async symlink(target: string, linkpath: string): Promise<void> {
    const normalizedLinkpath = this.normalizePath(linkpath);

    const existing = this.resolvePath(normalizedLinkpath);
    if (existing !== null) {
      throw createFsError({
        code: 'EEXIST',
        syscall: 'open',
        path: normalizedLinkpath,
        message: 'file already exists',
      });
    }

    const parent = this.resolveParent(normalizedLinkpath);
    if (!parent) {
      throw createFsError({
        code: 'ENOENT',
        syscall: 'open',
        path: normalizedLinkpath,
        message: 'no such file or directory',
      });
    }

    this.storage.transactionSync(() => {
      const mode = S_IFLNK | 0o777;
      const symlinkIno = this.createInode(mode);
      this.createDentry(parent.parentIno, parent.name, symlinkIno);

      this.storage.sql.exec(
        'INSERT INTO fs_symlink (ino, target) VALUES (?, ?)',
        symlinkIno, target
      );

      this.storage.sql.exec(
        'UPDATE fs_inode SET size = ? WHERE ino = ?',
        target.length, symlinkIno
      );
    });
  }

  async readlink(path: string): Promise<string> {
    const { normalizedPath, ino } = this.resolvePathOrThrow(path, 'open');

    const mode = this.getInodeMode(ino);
    if (mode === null || (mode & S_IFMT) !== S_IFLNK) {
      throw createFsError({
        code: 'EINVAL',
        syscall: 'open',
        path: normalizedPath,
        message: 'invalid argument',
      });
    }

    const rows = this.storage.sql.exec<{ target: string }>(
      'SELECT target FROM fs_symlink WHERE ino = ?',
      ino
    ).toArray();

    if (rows.length === 0) {
      throw createFsError({
        code: 'ENOENT',
        syscall: 'open',
        path: normalizedPath,
        message: 'no such file or directory',
      });
    }

    return rows[0].target;
  }

  async access(path: string): Promise<void> {
    const normalizedPath = this.normalizePath(path);
    const ino = this.resolvePath(normalizedPath);
    if (ino === null) {
      throw createFsError({
        code: 'ENOENT',
        syscall: 'access',
        path: normalizedPath,
        message: 'no such file or directory',
      });
    }
  }

  async statfs(): Promise<FilesystemStats> {
    const inodeRows = this.storage.sql.exec<{ count: number }>(
      'SELECT COUNT(*) as count FROM fs_inode'
    ).toArray();

    const bytesRows = this.storage.sql.exec<{ total: number }>(
      'SELECT COALESCE(SUM(LENGTH(data)), 0) as total FROM fs_data'
    ).toArray();

    return {
      inodes: inodeRows[0].count,
      bytesUsed: bytesRows[0].total,
    };
  }

  async open(path: string): Promise<FileHandle> {
    const { normalizedPath, ino } = this.resolvePathOrThrow(path, 'open');

    const mode = this.getInodeMode(ino);
    if (mode !== null && (mode & S_IFMT) === S_IFDIR) {
      throw createFsError({
        code: 'EISDIR',
        syscall: 'open',
        path: normalizedPath,
        message: 'illegal operation on a directory',
      });
    }

    return new AgentFSFile(this.storage, ino, this.chunkSize);
  }

  // Legacy alias
  async deleteFile(path: string): Promise<void> {
    return await this.unlink(path);
  }
}
