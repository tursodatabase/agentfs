package agentfs

// File type constants (upper bits of mode)
const (
	S_IFMT   = 0o170000 // File type mask
	S_IFREG  = 0o100000 // Regular file
	S_IFDIR  = 0o040000 // Directory
	S_IFLNK  = 0o120000 // Symbolic link
	S_IFIFO  = 0o010000 // FIFO/named pipe
	S_IFCHR  = 0o020000 // Character device
	S_IFBLK  = 0o060000 // Block device
	S_IFSOCK = 0o140000 // Socket
)

// Permission bits (lower 12 bits of mode)
const (
	S_IRWXU = 0o0700 // Owner read/write/execute
	S_IRUSR = 0o0400 // Owner read
	S_IWUSR = 0o0200 // Owner write
	S_IXUSR = 0o0100 // Owner execute

	S_IRWXG = 0o0070 // Group read/write/execute
	S_IRGRP = 0o0040 // Group read
	S_IWGRP = 0o0020 // Group write
	S_IXGRP = 0o0010 // Group execute

	S_IRWXO = 0o0007 // Others read/write/execute
	S_IROTH = 0o0004 // Others read
	S_IWOTH = 0o0002 // Others write
	S_IXOTH = 0o0001 // Others execute
)

// Default modes
const (
	DefaultFileMode = S_IFREG | 0o644 // Regular file: rw-r--r--
	DefaultDirMode  = S_IFDIR | 0o755 // Directory: rwxr-xr-x
)

// Default chunk size for file data storage
const DefaultChunkSize = 4096

// POSIX error codes
const (
	EPERM     = 1  // Operation not permitted
	ENOENT    = 2  // No such file or directory
	EIO       = 5  // I/O error
	EBADF     = 9  // Bad file descriptor
	EACCES    = 13 // Permission denied
	EEXIST    = 17 // File exists
	ENOTDIR   = 20 // Not a directory
	EISDIR    = 21 // Is a directory
	EINVAL    = 22 // Invalid argument
	ENOSPC    = 28 // No space left on device
	ENOSYS    = 38 // Function not implemented
	ENOTEMPTY = 39 // Directory not empty
	ELOOP     = 40 // Too many symbolic links
)

// Open flags (matching os package)
const (
	O_RDONLY = 0x0
	O_WRONLY = 0x1
	O_RDWR   = 0x2
	O_APPEND = 0x400
	O_CREATE = 0x40
	O_EXCL   = 0x80
	O_TRUNC  = 0x200
)

// Root inode number
const RootIno = 1
