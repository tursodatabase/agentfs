"""Filesystem constants"""

# File types for mode field
S_IFMT = 0o170000  # File type mask
S_IFREG = 0o100000  # Regular file
S_IFDIR = 0o040000  # Directory
S_IFLNK = 0o120000  # Symbolic link

# Default permissions
DEFAULT_FILE_MODE = S_IFREG | 0o644  # Regular file, rw-r--r--
DEFAULT_DIR_MODE = S_IFDIR | 0o755  # Directory, rwxr-xr-x

DEFAULT_CHUNK_SIZE = 4096
