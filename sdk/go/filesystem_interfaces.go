package agentfs

// File types for mode field
const S_IFMT = 0o170000  // File type mask
const S_IFREG = 0o100000 // Regular file
const S_IFDIR = 0o040000 // Directory
const S_IFLNK = 0o120000 // Symbolic link

// Default permissions
const DEFAULT_FILE_MODE = S_IFREG | 0o644 // Regular file, rw-r--r--
const DEFAULT_DIR_MODE = S_IFDIR | 0o755  // Directory, rwxr-xr-x

type Stats interface {
	IsFile() bool
	IsDirectory() bool
	IsSymbolicLink() bool
}

// Implements Stats
type DataStats struct {
	Ino   int
	Mode  int
	Nlink int
	Uid   int
	Gid   int
	Size  int
	Atime int
	Mtime int
	Ctime int
}

func (s DataStats) IsFile() bool {
	return s.Mode&S_IFMT == S_IFREG
}

func (s DataStats) IsDirectory() bool {
	return s.Mode&S_IFMT == S_IFDIR
}

func (s DataStats) IsSymbolicLink() bool {
	return s.Mode&S_IFMT == S_IFLNK
}

func NewDataStats(ino, mode, nlink, uid, gid, size, atime, mtime, ctime int) DataStats {
	return DataStats{
		Ino:   ino,
		Mode:  mode,
		Nlink: nlink,
		Uid:   uid,
		Gid:   gid,
		Size:  size,
		Atime: atime,
		Mtime: mtime,
		Ctime: ctime,
	}
}

/**
 * Directory entry with full statistics
 */
type DirEntry struct {
	/** Entry name (without path) */
	name string
	/** Full statistics for this entry */
	stats Stats
}

/**
 * Filesystem statistics
 */
type FilesystemStats struct {
	/** Total number of inodes (files, directories, symlinks) */
	Inodes int
	/** Total bytes used by file contents */
	BytesUsed int
}

/**
 * An open file handle for performing I/O operations.
 * Similar to Node.js FileHandle from fs/promises.
 */
type FileHandle interface {
	/**
	 * Read from the file at the given offset.
	 */
	Pread(int, int) ([]byte, error)

	/**
	 * Write to the file at the given offset.
	 */
	Pwrite(int, []byte) error

	/**
	 * Truncate the file to the specified size.
	 */
	Truncate(int) error

	/**
	 * Synchronize file data to persistent storage.
	 */
	Fsync() error

	/**
	 * Get file statistics.
	 */
	Fstat() (Stats, error)
}

type RmOptions struct {
	Recursive bool
	Force     bool
}

type EncodingOption struct {
	Encoding string
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
type FileSystem interface {
	/**
	 * Get file statistics.
	 */
	Stat(string) (Stats, error)

	/**
	 * Get file statistics without following symlinks.
	 */
	Lstat(string) (Stats, error)

	/**
	 * Read entire file contents.
	 */
	ReadFile(string) ([]byte, error)

	/**
	 * Write data to a file (creates or overwrites).
	 * Creates parent directories if they don't exist.
	 */
	WriteFile(
		string,
		[]byte,
	) error

	/**
	 * List directory contents.
	 * @throws {ErrnoException} ENOENT if directory does not exist
	 * @throws {ErrnoException} ENOTDIR if path is not a directory
	 */
	Readdir(string) ([]string, error)

	/**
	 * List directory contents with full statistics for each entry.
	 * Optimized version that avoids N+1 queries.
	 * @throws {ErrnoException} ENOENT if directory does not exist
	 * @throws {ErrnoException} ENOTDIR if path is not a directory
	 */
	ReaddirPlus(string) ([]DirEntry, error)

	/**
	 * Create a directory.
	 * @throws {ErrnoException} EEXIST if path already exists
	 * @throws {ErrnoException} ENOENT if parent does not exist
	 */
	Mkdir(string) error

	/**
	 * Remove an empty directory.
	 * @throws {ErrnoException} ENOENT if path does not exist
	 * @throws {ErrnoException} ENOTEMPTY if directory is not empty
	 * @throws {ErrnoException} ENOTDIR if path is not a directory
	 */
	Rmdir(string) error

	/**
	 * Remove a file.
	 * @throws {ErrnoException} ENOENT if path does not exist
	 * @throws {ErrnoException} EISDIR if path is a directory
	 */
	Unlink(string) error

	/**
	 * Remove a file or directory.
	 */
	Rm(string, ...RmOptions) error

	/**
	 * Rename/move a file or directory.
	 * @throws {ErrnoException} ENOENT if source does not exist
	 */
	Rename(string, string) error

	/**
	 * Copy a file.
	 * @throws {ErrnoException} ENOENT if source does not exist
	 * @throws {ErrnoException} EISDIR if source or dest is a directory
	 */
	CopyFile(string, string) error

	/**
	 * Create a symbolic link.
	 * @throws {ErrnoException} EEXIST if linkpath already exists
	 */
	Symlink(string, string) error

	/**
	 * Read the target of a symbolic link.
	 * @throws {ErrnoException} ENOENT if path does not exist
	 * @throws {ErrnoException} EINVAL if path is not a symlink
	 */
	Readlink(string) (string, error)

	/**
	 * Test file access (existence check).
	 * @throws {ErrnoException} ENOENT if path does not exist
	 */
	Access(string) error

	/**
	 * Get filesystem statistics.
	 */
	Statfs() (FilesystemStats, error)

	/**
	 * Open a file and return a file handle for I/O operations.
	 * @throws {ErrnoException} ENOENT if file does not exist
	 */
	Open(string) (FileHandle, error)
}
