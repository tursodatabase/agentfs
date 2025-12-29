// File types for mode field
export const S_IFMT = 0o170000;   // File type mask
export const S_IFREG = 0o100000;  // Regular file
export const S_IFDIR = 0o040000;  // Directory
export const S_IFLNK = 0o120000;  // Symbolic link

// Default permissions
export const DEFAULT_FILE_MODE = S_IFREG | 0o644;  // Regular file, rw-r--r--
export const DEFAULT_DIR_MODE = S_IFDIR | 0o755;   // Directory, rwxr-xr-x

/**
 * File statistics (Node.js compatible)
 */
export interface Stats {
  ino: number;
  mode: number;
  nlink: number;
  uid: number;
  gid: number;
  size: number;
  atime: number;
  mtime: number;
  ctime: number;
  isFile(): boolean;
  isDirectory(): boolean;
  isSymbolicLink(): boolean;
}

/**
 * Plain stats data without methods (for creating Stats objects)
 */
export interface StatsData {
  ino: number;
  mode: number;
  nlink: number;
  uid: number;
  gid: number;
  size: number;
  atime: number;
  mtime: number;
  ctime: number;
}

/**
 * Create a Stats object from raw data
 */
export function createStats(data: StatsData): Stats {
  return {
    ...data,
    isFile: () => (data.mode & S_IFMT) === S_IFREG,
    isDirectory: () => (data.mode & S_IFMT) === S_IFDIR,
    isSymbolicLink: () => (data.mode & S_IFMT) === S_IFLNK,
  };
}

/**
 * Directory entry with full statistics
 */
export interface DirEntry {
  /** Entry name (without path) */
  name: string;
  /** Full statistics for this entry */
  stats: Stats;
}

/**
 * Filesystem statistics
 */
export interface FilesystemStats {
  /** Total number of inodes (files, directories, symlinks) */
  inodes: number;
  /** Total bytes used by file contents */
  bytesUsed: number;
}

/**
 * An open file handle for performing I/O operations.
 * Similar to Node.js FileHandle from fs/promises.
 */
export interface FileHandle {
  /**
   * Read from the file at the given offset.
   */
  pread(offset: number, size: number): Promise<Buffer>;

  /**
   * Write to the file at the given offset.
   */
  pwrite(offset: number, data: Buffer): Promise<void>;

  /**
   * Truncate the file to the specified size.
   */
  truncate(size: number): Promise<void>;

  /**
   * Synchronize file data to persistent storage.
   */
  fsync(): Promise<void>;

  /**
   * Get file statistics.
   */
  fstat(): Promise<Stats>;
}

/**
 * FileSystem interface following Node.js fs/promises API conventions.
 *
 * This interface abstracts over different filesystem backends,
 * allowing implementations like AgentFS (SQLite-backed), HostFS (native filesystem),
 * and OverlayFS (layered filesystem).
 *
 * Methods throw errors on failure (ENOENT, EEXIST, etc.) like Node.js fs/promises.
 */
export interface FileSystem {
  /**
   * Get file statistics.
   * @throws {ErrnoException} ENOENT if path does not exist
   */
  stat(path: string): Promise<Stats>;

  /**
   * Get file statistics without following symlinks.
   * @throws {ErrnoException} ENOENT if path does not exist
   */
  lstat(path: string): Promise<Stats>;

  /**
   * Read entire file contents.
   * @throws {ErrnoException} ENOENT if file does not exist
   * @throws {ErrnoException} EISDIR if path is a directory
   */
  readFile(path: string): Promise<Buffer>;
  readFile(path: string, encoding: BufferEncoding): Promise<string>;
  readFile(path: string, options: { encoding: BufferEncoding }): Promise<string>;
  readFile(path: string, options?: BufferEncoding | { encoding?: BufferEncoding }): Promise<Buffer | string>;

  /**
   * Write data to a file (creates or overwrites).
   * Creates parent directories if they don't exist.
   */
  writeFile(
    path: string,
    data: string | Buffer,
    options?: BufferEncoding | { encoding?: BufferEncoding }
  ): Promise<void>;

  /**
   * List directory contents.
   * @throws {ErrnoException} ENOENT if directory does not exist
   * @throws {ErrnoException} ENOTDIR if path is not a directory
   */
  readdir(path: string): Promise<string[]>;

  /**
   * List directory contents with full statistics for each entry.
   * Optimized version that avoids N+1 queries.
   * @throws {ErrnoException} ENOENT if directory does not exist
   * @throws {ErrnoException} ENOTDIR if path is not a directory
   */
  readdirPlus(path: string): Promise<DirEntry[]>;

  /**
   * Create a directory.
   * @throws {ErrnoException} EEXIST if path already exists
   * @throws {ErrnoException} ENOENT if parent does not exist
   */
  mkdir(path: string): Promise<void>;

  /**
   * Remove an empty directory.
   * @throws {ErrnoException} ENOENT if path does not exist
   * @throws {ErrnoException} ENOTEMPTY if directory is not empty
   * @throws {ErrnoException} ENOTDIR if path is not a directory
   */
  rmdir(path: string): Promise<void>;

  /**
   * Remove a file.
   * @throws {ErrnoException} ENOENT if path does not exist
   * @throws {ErrnoException} EISDIR if path is a directory
   */
  unlink(path: string): Promise<void>;

  /**
   * Remove a file or directory.
   */
  rm(path: string, options?: { force?: boolean; recursive?: boolean }): Promise<void>;

  /**
   * Rename/move a file or directory.
   * @throws {ErrnoException} ENOENT if source does not exist
   */
  rename(oldPath: string, newPath: string): Promise<void>;

  /**
   * Copy a file.
   * @throws {ErrnoException} ENOENT if source does not exist
   * @throws {ErrnoException} EISDIR if source or dest is a directory
   */
  copyFile(src: string, dest: string): Promise<void>;

  /**
   * Create a symbolic link.
   * @throws {ErrnoException} EEXIST if linkpath already exists
   */
  symlink(target: string, linkpath: string): Promise<void>;

  /**
   * Read the target of a symbolic link.
   * @throws {ErrnoException} ENOENT if path does not exist
   * @throws {ErrnoException} EINVAL if path is not a symlink
   */
  readlink(path: string): Promise<string>;

  /**
   * Test file access (existence check).
   * @throws {ErrnoException} ENOENT if path does not exist
   */
  access(path: string): Promise<void>;

  /**
   * Get filesystem statistics.
   */
  statfs(): Promise<FilesystemStats>;

  /**
   * Open a file and return a file handle for I/O operations.
   * @throws {ErrnoException} ENOENT if file does not exist
   */
  open(path: string): Promise<FileHandle>;
}
