"""Filesystem implementation"""

import time
from dataclasses import dataclass
from typing import List, Optional, Union

from turso.aio import Connection

from .constants import (
    DEFAULT_CHUNK_SIZE,
    DEFAULT_DIR_MODE,
    DEFAULT_FILE_MODE,
    S_IFDIR,
    S_IFLNK,
    S_IFMT,
    S_IFREG,
)
from .errors import ErrnoException, FsSyscall
from .guards import (
    assert_inode_is_directory,
    assert_not_root,
    assert_not_symlink_mode,
    assert_readable_existing_inode,
    assert_readdir_target_inode,
    assert_unlink_target_inode,
    assert_writable_existing_inode,
    get_inode_mode_or_throw,
    normalize_rm_options,
    throw_enoent_unless_force,
)

# Re-export constants for backwards compatibility
__all__ = ["Filesystem", "Stats", "S_IFMT", "S_IFREG", "S_IFDIR", "S_IFLNK"]


@dataclass
class Stats:
    """File/directory statistics

    Attributes:
        ino: Inode number
        mode: File mode and permissions
        nlink: Number of hard links
        uid: User ID
        gid: Group ID
        size: File size in bytes
        atime: Access time (Unix timestamp)
        mtime: Modification time (Unix timestamp)
        ctime: Change time (Unix timestamp)
    """

    ino: int
    mode: int
    nlink: int
    uid: int
    gid: int
    size: int
    atime: int
    mtime: int
    ctime: int

    def is_file(self) -> bool:
        """Check if this is a regular file"""
        return (self.mode & S_IFMT) == S_IFREG

    def is_directory(self) -> bool:
        """Check if this is a directory"""
        return (self.mode & S_IFMT) == S_IFDIR

    def is_symbolic_link(self) -> bool:
        """Check if this is a symbolic link"""
        return (self.mode & S_IFMT) == S_IFLNK


class Filesystem:
    """Virtual filesystem backed by SQLite

    Provides a POSIX-like filesystem interface with support for
    files, directories, and symbolic links.
    """

    def __init__(self, db: Connection):
        """Private constructor - use Filesystem.from_database() instead"""
        self._db = db
        self._root_ino = 1
        self._chunk_size = DEFAULT_CHUNK_SIZE

    @staticmethod
    async def from_database(db: Connection) -> "Filesystem":
        """Create a Filesystem from an existing database connection

        Args:
            db: An existing pyturso.aio Connection

        Returns:
            Fully initialized Filesystem instance
        """
        fs = Filesystem(db)
        await fs._initialize()
        return fs

    def get_chunk_size(self) -> int:
        """Get the configured chunk size"""
        return self._chunk_size

    async def _initialize(self) -> None:
        """Initialize the database schema"""
        # Create all tables
        await self._db.executescript("""
            CREATE TABLE IF NOT EXISTS fs_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

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
            );

            CREATE TABLE IF NOT EXISTS fs_dentry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                parent_ino INTEGER NOT NULL,
                ino INTEGER NOT NULL,
                UNIQUE(parent_ino, name)
            );

            CREATE INDEX IF NOT EXISTS idx_fs_dentry_parent
            ON fs_dentry(parent_ino, name);

            CREATE TABLE IF NOT EXISTS fs_data (
                ino INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                data BLOB NOT NULL,
                PRIMARY KEY (ino, chunk_index)
            );

            CREATE TABLE IF NOT EXISTS fs_symlink (
                ino INTEGER PRIMARY KEY,
                target TEXT NOT NULL
            );
        """)
        await self._db.commit()

        # Initialize config and root directory
        self._chunk_size = await self._ensure_root()

    async def _ensure_root(self) -> int:
        """Ensure config and root directory exist, returns the chunk_size"""
        # Ensure chunk_size config exists and get its value
        cursor = await self._db.execute("SELECT value FROM fs_config WHERE key = 'chunk_size'")
        config = await cursor.fetchone()

        if not config:
            await self._db.execute(
                "INSERT INTO fs_config (key, value) VALUES ('chunk_size', ?)",
                (str(DEFAULT_CHUNK_SIZE),),
            )
            await self._db.commit()
            chunk_size = DEFAULT_CHUNK_SIZE
        else:
            chunk_size = int(config[0]) if config[0] else DEFAULT_CHUNK_SIZE

        # Ensure root directory exists
        cursor = await self._db.execute("SELECT ino FROM fs_inode WHERE ino = ?", (self._root_ino,))
        root = await cursor.fetchone()

        if not root:
            now = int(time.time())
            await self._db.execute(
                """
                INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
                VALUES (?, ?, 1, 0, 0, 0, ?, ?, ?)
                """,
                (self._root_ino, DEFAULT_DIR_MODE, now, now, now),
            )
            await self._db.commit()

        return chunk_size

    def _normalize_path(self, path: str) -> str:
        """Normalize a path"""
        # Remove trailing slashes except for root
        normalized = path.rstrip("/") or "/"
        # Ensure leading slash
        return normalized if normalized.startswith("/") else "/" + normalized

    def _split_path(self, path: str) -> List[str]:
        """Split path into components"""
        normalized = self._normalize_path(path)
        if normalized == "/":
            return []
        return [p for p in normalized.split("/") if p]

    async def _resolve_path(self, path: str) -> Optional[int]:
        """Resolve a path to an inode number"""
        normalized = self._normalize_path(path)

        # Root directory
        if normalized == "/":
            return self._root_ino

        parts = self._split_path(normalized)
        current_ino = self._root_ino

        # Traverse the path
        for name in parts:
            cursor = await self._db.execute(
                """
                SELECT ino FROM fs_dentry
                WHERE parent_ino = ? AND name = ?
                """,
                (current_ino, name),
            )
            result = await cursor.fetchone()

            if not result:
                return None

            current_ino = result[0]

        return current_ino

    async def _resolve_parent(self, path: str) -> Optional[tuple]:
        """Get parent directory inode and basename from path"""
        normalized = self._normalize_path(path)

        if normalized == "/":
            return None  # Root has no parent

        parts = self._split_path(normalized)
        name = parts[-1]
        parent_path = "/" if len(parts) == 1 else "/" + "/".join(parts[:-1])

        parent_ino = await self._resolve_path(parent_path)

        if parent_ino is None:
            return None

        return (parent_ino, name)

    async def _create_inode(self, mode: int, uid: int = 0, gid: int = 0) -> int:
        """Create an inode

        Note: We use RETURNING clause which requires explicit cursor close
        when working with CDC-enabled TursoDB connections.
        """
        now = int(time.time())
        cursor = self._db.cursor()
        try:
            await cursor.execute(
                """
                INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
                VALUES (?, ?, ?, 0, ?, ?, ?)
                RETURNING ino
                """,
                (mode, uid, gid, now, now, now),
            )
            row = await cursor.fetchone()
            assert row is not None
            ino = row[0]
        finally:
            await cursor.close()
        # Commit after cursor is closed
        await self._db.commit()
        return ino

    async def _create_dentry(self, parent_ino: int, name: str, ino: int) -> None:
        """Create a directory entry"""
        await self._db.execute(
            """
            INSERT INTO fs_dentry (name, parent_ino, ino)
            VALUES (?, ?, ?)
            """,
            (name, parent_ino, ino),
        )
        # Increment link count
        await self._db.execute(
            "UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?",
            (ino,),
        )
        await self._db.commit()

    async def _ensure_parent_dirs(self, path: str) -> None:
        """Ensure parent directories exist"""
        parts = self._split_path(path)

        # Remove the filename, keep only directory parts
        parts = parts[:-1]

        current_ino = self._root_ino
        current_path = ""

        for name in parts:
            current_path += "/" + name

            # Check if this directory exists
            cursor = await self._db.execute(
                """
                SELECT ino FROM fs_dentry
                WHERE parent_ino = ? AND name = ?
                """,
                (current_ino, name),
            )
            result = await cursor.fetchone()

            if not result:
                # Create directory
                dir_ino = await self._create_inode(DEFAULT_DIR_MODE)
                await self._create_dentry(current_ino, name, dir_ino)
                current_ino = dir_ino
            else:
                current_ino = result[0]

    async def _get_link_count(self, ino: int) -> int:
        """Get link count for an inode"""
        cursor = await self._db.execute("SELECT nlink FROM fs_inode WHERE ino = ?", (ino,))
        result = await cursor.fetchone()
        return result[0] if result else 0

    async def _get_inode_mode(self, ino: int) -> Optional[int]:
        """Get mode for an inode"""
        cursor = await self._db.execute("SELECT mode FROM fs_inode WHERE ino = ?", (ino,))
        row = await cursor.fetchone()
        return row[0] if row else None

    async def _resolve_path_or_throw(self, path: str, syscall: FsSyscall) -> tuple[str, int]:
        """Resolve path to inode or throw ENOENT"""
        normalized_path = self._normalize_path(path)
        ino = await self._resolve_path(normalized_path)
        if ino is None:
            raise ErrnoException(
                code="ENOENT",
                syscall=syscall,
                path=normalized_path,
                message="no such file or directory",
            )
        return (normalized_path, ino)

    async def write_file(
        self,
        path: str,
        content: Union[str, bytes],
        encoding: str = "utf-8",
    ) -> None:
        """Write content to a file

        Args:
            path: Path to the file
            content: Content to write (string or bytes)
            encoding: Text encoding (default: 'utf-8')

        Example:
            >>> await fs.write_file('/data/config.json', '{"key": "value"}')
        """
        # Ensure parent directories exist
        await self._ensure_parent_dirs(path)

        normalized_path = self._normalize_path(path)
        # Check if file already exists
        ino = await self._resolve_path(normalized_path)

        if ino is not None:
            # Validate existing inode
            await assert_writable_existing_inode(self._db, ino, "open", normalized_path)
            # Update existing file
            await self._update_file_content(ino, content, encoding)
        else:
            # Create new file
            parent = await self._resolve_parent(normalized_path)
            if not parent:
                raise ErrnoException(
                    code="ENOENT",
                    syscall="open",
                    path=normalized_path,
                    message="no such file or directory",
                )

            parent_ino, name = parent

            # Ensure parent is a directory
            await assert_inode_is_directory(self._db, parent_ino, "open", normalized_path)

            # Create inode
            file_ino = await self._create_inode(DEFAULT_FILE_MODE)

            # Create directory entry
            await self._create_dentry(parent_ino, name, file_ino)

            # Write content
            await self._update_file_content(file_ino, content, encoding)

    async def _update_file_content(
        self, ino: int, content: Union[str, bytes], encoding: str = "utf-8"
    ) -> None:
        """Update file content"""
        buffer = content.encode(encoding) if isinstance(content, str) else content
        now = int(time.time())

        # Delete existing data chunks
        await self._db.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))

        # Write data in chunks
        if len(buffer) > 0:
            chunk_index = 0
            for offset in range(0, len(buffer), self._chunk_size):
                chunk = buffer[offset : min(offset + self._chunk_size, len(buffer))]
                await self._db.execute(
                    """
                    INSERT INTO fs_data (ino, chunk_index, data)
                    VALUES (?, ?, ?)
                    """,
                    (ino, chunk_index, chunk),
                )
                chunk_index += 1

        # Update inode size and mtime
        await self._db.execute(
            """
            UPDATE fs_inode
            SET size = ?, mtime = ?
            WHERE ino = ?
            """,
            (len(buffer), now, ino),
        )
        await self._db.commit()

    async def read_file(self, path: str, encoding: Optional[str] = "utf-8") -> Union[bytes, str]:
        """Read content from a file

        Args:
            path: Path to the file
            encoding: Text encoding (default: 'utf-8'). Set to None to return bytes.

        Returns:
            File content as string (if encoding specified) or bytes

        Example:
            >>> content = await fs.read_file('/data/config.json')
            >>> data = await fs.read_file('/data/image.png', encoding=None)
        """
        normalized_path, ino = await self._resolve_path_or_throw(path, "open")

        await assert_readable_existing_inode(self._db, ino, "open", normalized_path)

        # Get all data chunks
        cursor = await self._db.execute(
            """
            SELECT data FROM fs_data
            WHERE ino = ?
            ORDER BY chunk_index ASC
            """,
            (ino,),
        )
        rows = await cursor.fetchall()

        if not rows:
            combined = b""
        else:
            # Concatenate all chunks
            combined = b"".join(row[0] for row in rows)

        # Update atime
        now = int(time.time())
        await self._db.execute("UPDATE fs_inode SET atime = ? WHERE ino = ?", (now, ino))
        await self._db.commit()

        if encoding:
            return combined.decode(encoding)
        return combined

    async def readdir(self, path: str) -> List[str]:
        """List directory contents

        Args:
            path: Path to the directory

        Returns:
            List of entry names

        Example:
            >>> entries = await fs.readdir('/data')
            >>> for entry in entries:
            >>>     print(entry)
        """
        normalized_path, ino = await self._resolve_path_or_throw(path, "scandir")

        await assert_readdir_target_inode(self._db, ino, normalized_path)

        # Get all directory entries
        cursor = await self._db.execute(
            """
            SELECT name FROM fs_dentry
            WHERE parent_ino = ?
            ORDER BY name ASC
            """,
            (ino,),
        )
        rows = await cursor.fetchall()

        return [row[0] for row in rows]

    async def unlink(self, path: str) -> None:
        """Delete a file (unlink)

        Args:
            path: Path to the file

        Example:
            >>> await fs.unlink('/data/temp.txt')
        """
        normalized_path = self._normalize_path(path)
        assert_not_root(normalized_path, "unlink")
        normalized_path, ino = await self._resolve_path_or_throw(normalized_path, "unlink")

        await assert_unlink_target_inode(self._db, ino, normalized_path)

        parent = await self._resolve_parent(normalized_path)
        # parent is guaranteed to exist here since normalized_path != '/'
        assert parent is not None
        parent_ino, name = parent

        # Delete the directory entry
        await self._db.execute(
            """
            DELETE FROM fs_dentry
            WHERE parent_ino = ? AND name = ?
            """,
            (parent_ino, name),
        )

        # Decrement link count
        await self._db.execute(
            "UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?",
            (ino,),
        )

        # Check if this was the last link to the inode
        link_count = await self._get_link_count(ino)
        if link_count == 0:
            # Delete the inode
            await self._db.execute("DELETE FROM fs_inode WHERE ino = ?", (ino,))

            # Delete all data chunks
            await self._db.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))

        await self._db.commit()

    # Backwards-compatible alias
    async def delete_file(self, path: str) -> None:
        """Delete a file (deprecated, use unlink instead)

        Args:
            path: Path to the file

        Example:
            >>> await fs.delete_file('/data/temp.txt')
        """
        return await self.unlink(path)

    async def stat(self, path: str) -> Stats:
        """Get file/directory statistics

        Args:
            path: Path to the file or directory

        Returns:
            Stats object with file information

        Example:
            >>> stats = await fs.stat('/data/config.json')
            >>> print(f"Size: {stats.size} bytes")
            >>> print(f"Is file: {stats.is_file()}")
        """
        normalized_path, ino = await self._resolve_path_or_throw(path, "stat")

        cursor = await self._db.execute(
            """
            SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime
            FROM fs_inode
            WHERE ino = ?
            """,
            (ino,),
        )
        row = await cursor.fetchone()

        if not row:
            raise ErrnoException(
                code="ENOENT",
                syscall="stat",
                path=normalized_path,
                message="no such file or directory",
            )

        return Stats(
            ino=row[0],
            mode=row[1],
            nlink=row[2],
            uid=row[3],
            gid=row[4],
            size=row[5],
            atime=row[6],
            mtime=row[7],
            ctime=row[8],
        )

    async def mkdir(self, path: str) -> None:
        """Create a directory (non-recursive)

        Args:
            path: Path to the directory to create

        Example:
            >>> await fs.mkdir('/data/new_dir')
        """
        normalized_path = self._normalize_path(path)

        existing = await self._resolve_path(normalized_path)
        if existing is not None:
            raise ErrnoException(
                code="EEXIST",
                syscall="mkdir",
                path=normalized_path,
                message="file already exists",
            )

        parent = await self._resolve_parent(normalized_path)
        if not parent:
            raise ErrnoException(
                code="ENOENT",
                syscall="mkdir",
                path=normalized_path,
                message="no such file or directory",
            )

        parent_ino, name = parent
        await assert_inode_is_directory(self._db, parent_ino, "mkdir", normalized_path)

        dir_ino = await self._create_inode(DEFAULT_DIR_MODE)
        try:
            await self._create_dentry(parent_ino, name, dir_ino)
        except Exception:
            raise ErrnoException(
                code="EEXIST",
                syscall="mkdir",
                path=normalized_path,
                message="file already exists",
            )

    async def rmdir(self, path: str) -> None:
        """Remove an empty directory

        Args:
            path: Path to the directory to remove

        Example:
            >>> await fs.rmdir('/data/empty_dir')
        """
        normalized_path = self._normalize_path(path)
        assert_not_root(normalized_path, "rmdir")

        normalized_path, ino = await self._resolve_path_or_throw(normalized_path, "rmdir")

        mode = await get_inode_mode_or_throw(self._db, ino, "rmdir", normalized_path)
        assert_not_symlink_mode(mode, "rmdir", normalized_path)
        if (mode & S_IFMT) != S_IFDIR:
            raise ErrnoException(
                code="ENOTDIR",
                syscall="rmdir",
                path=normalized_path,
                message="not a directory",
            )

        cursor = await self._db.execute(
            """
            SELECT 1 as one FROM fs_dentry
            WHERE parent_ino = ?
            LIMIT 1
            """,
            (ino,),
        )
        child = await cursor.fetchone()
        if child:
            raise ErrnoException(
                code="ENOTEMPTY",
                syscall="rmdir",
                path=normalized_path,
                message="directory not empty",
            )

        parent = await self._resolve_parent(normalized_path)
        if not parent:
            raise ErrnoException(
                code="EPERM",
                syscall="rmdir",
                path=normalized_path,
                message="operation not permitted",
            )

        parent_ino, name = parent
        await self._remove_dentry_and_maybe_inode(parent_ino, name, ino)

    async def rm(
        self,
        path: str,
        force: bool = False,
        recursive: bool = False,
    ) -> None:
        """Remove a file or directory

        Args:
            path: Path to remove
            force: If True, ignore nonexistent files
            recursive: If True, remove directories and their contents recursively

        Example:
            >>> await fs.rm('/data/file.txt')
            >>> await fs.rm('/data/dir', recursive=True)
        """
        normalized_path = self._normalize_path(path)
        options = normalize_rm_options({"force": force, "recursive": recursive})
        force = options["force"]
        recursive = options["recursive"]
        assert_not_root(normalized_path, "rm")

        ino = await self._resolve_path(normalized_path)
        if ino is None:
            throw_enoent_unless_force(normalized_path, "rm", force)
            return

        mode = await get_inode_mode_or_throw(self._db, ino, "rm", normalized_path)
        assert_not_symlink_mode(mode, "rm", normalized_path)

        parent = await self._resolve_parent(normalized_path)
        if not parent:
            raise ErrnoException(
                code="EPERM",
                syscall="rm",
                path=normalized_path,
                message="operation not permitted",
            )

        parent_ino, name = parent

        if (mode & S_IFMT) == S_IFDIR:
            if not recursive:
                raise ErrnoException(
                    code="EISDIR",
                    syscall="rm",
                    path=normalized_path,
                    message="illegal operation on a directory",
                )

            await self._rm_dir_contents_recursive(ino)
            await self._remove_dentry_and_maybe_inode(parent_ino, name, ino)
            return

        # Regular file
        await self._remove_dentry_and_maybe_inode(parent_ino, name, ino)

    async def _rm_dir_contents_recursive(self, dir_ino: int) -> None:
        """Recursively remove directory contents"""
        cursor = await self._db.execute(
            """
            SELECT name, ino FROM fs_dentry
            WHERE parent_ino = ?
            ORDER BY name ASC
            """,
            (dir_ino,),
        )
        children = await cursor.fetchall()

        for name, child_ino in children:
            mode = await self._get_inode_mode(child_ino)
            if mode is None:
                # DB inconsistency; treat as already gone
                continue

            if (mode & S_IFMT) == S_IFDIR:
                await self._rm_dir_contents_recursive(child_ino)
                await self._remove_dentry_and_maybe_inode(dir_ino, name, child_ino)
            else:
                # Not supported yet (symlinks)
                assert_not_symlink_mode(mode, "rm", "<symlink>")
                await self._remove_dentry_and_maybe_inode(dir_ino, name, child_ino)

    async def _remove_dentry_and_maybe_inode(self, parent_ino: int, name: str, ino: int) -> None:
        """Remove directory entry and inode if last link"""
        await self._db.execute(
            """
            DELETE FROM fs_dentry
            WHERE parent_ino = ? AND name = ?
            """,
            (parent_ino, name),
        )

        # Decrement link count
        await self._db.execute(
            "UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?",
            (ino,),
        )

        link_count = await self._get_link_count(ino)
        if link_count == 0:
            await self._db.execute("DELETE FROM fs_inode WHERE ino = ?", (ino,))
            await self._db.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))

        await self._db.commit()

    async def rename(self, old_path: str, new_path: str) -> None:
        """Rename (move) a file or directory

        Args:
            old_path: Current path
            new_path: New path

        Example:
            >>> await fs.rename('/data/old.txt', '/data/new.txt')
        """
        old_normalized = self._normalize_path(old_path)
        new_normalized = self._normalize_path(new_path)

        # No-op
        if old_normalized == new_normalized:
            return

        assert_not_root(old_normalized, "rename")
        assert_not_root(new_normalized, "rename")

        old_parent = await self._resolve_parent(old_normalized)
        if not old_parent:
            raise ErrnoException(
                code="EPERM",
                syscall="rename",
                path=old_normalized,
                message="operation not permitted",
            )

        new_parent = await self._resolve_parent(new_normalized)
        if not new_parent:
            raise ErrnoException(
                code="ENOENT",
                syscall="rename",
                path=new_normalized,
                message="no such file or directory",
            )

        new_parent_ino, new_name = new_parent

        # Ensure destination parent exists and is a directory
        await assert_inode_is_directory(self._db, new_parent_ino, "rename", new_normalized)

        # Begin transaction
        # Note: turso.aio doesn't support explicit BEGIN, but execute should be atomic
        try:
            old_normalized, old_ino = await self._resolve_path_or_throw(old_normalized, "rename")
            old_mode = await get_inode_mode_or_throw(self._db, old_ino, "rename", old_normalized)
            assert_not_symlink_mode(old_mode, "rename", old_normalized)
            old_is_dir = (old_mode & S_IFMT) == S_IFDIR

            # Prevent renaming a directory into its own subtree (would create cycles)
            if old_is_dir and new_normalized.startswith(old_normalized + "/"):
                raise ErrnoException(
                    code="EINVAL",
                    syscall="rename",
                    path=new_normalized,
                    message="invalid argument",
                )

            new_ino = await self._resolve_path(new_normalized)
            if new_ino is not None:
                new_mode = await get_inode_mode_or_throw(
                    self._db, new_ino, "rename", new_normalized
                )
                assert_not_symlink_mode(new_mode, "rename", new_normalized)
                new_is_dir = (new_mode & S_IFMT) == S_IFDIR

                if new_is_dir and not old_is_dir:
                    raise ErrnoException(
                        code="EISDIR",
                        syscall="rename",
                        path=new_normalized,
                        message="illegal operation on a directory",
                    )
                if not new_is_dir and old_is_dir:
                    raise ErrnoException(
                        code="ENOTDIR",
                        syscall="rename",
                        path=new_normalized,
                        message="not a directory",
                    )

                # If replacing a directory, it must be empty
                if new_is_dir:
                    cursor = await self._db.execute(
                        """
                        SELECT 1 as one FROM fs_dentry
                        WHERE parent_ino = ?
                        LIMIT 1
                        """,
                        (new_ino,),
                    )
                    child = await cursor.fetchone()
                    if child:
                        raise ErrnoException(
                            code="ENOTEMPTY",
                            syscall="rename",
                            path=new_normalized,
                            message="directory not empty",
                        )

                # Remove the destination entry (and inode if this was the last link)
                await self._remove_dentry_and_maybe_inode(new_parent_ino, new_name, new_ino)

            # Move the directory entry
            old_parent_ino, old_name = old_parent
            await self._db.execute(
                """
                UPDATE fs_dentry
                SET parent_ino = ?, name = ?
                WHERE parent_ino = ? AND name = ?
                """,
                (new_parent_ino, new_name, old_parent_ino, old_name),
            )

            # Update timestamps
            now = int(time.time())
            await self._db.execute(
                """
                UPDATE fs_inode
                SET ctime = ?
                WHERE ino = ?
                """,
                (now, old_ino),
            )

            await self._db.execute(
                """
                UPDATE fs_inode
                SET mtime = ?, ctime = ?
                WHERE ino = ?
                """,
                (now, now, old_parent_ino),
            )
            if new_parent_ino != old_parent_ino:
                await self._db.execute(
                    """
                    UPDATE fs_inode
                    SET mtime = ?, ctime = ?
                    WHERE ino = ?
                    """,
                    (now, now, new_parent_ino),
                )

            await self._db.commit()
        except Exception:
            # turso.aio doesn't have explicit rollback, changes are rolled back automatically
            raise

    async def copy_file(self, src: str, dest: str) -> None:
        """Copy a file. Overwrites destination if it exists.

        Args:
            src: Source file path
            dest: Destination file path

        Example:
            >>> await fs.copy_file('/data/src.txt', '/data/dest.txt')
        """
        src_normalized = self._normalize_path(src)
        dest_normalized = self._normalize_path(dest)

        if src_normalized == dest_normalized:
            raise ErrnoException(
                code="EINVAL",
                syscall="copyfile",
                path=dest_normalized,
                message="invalid argument",
            )

        # Resolve and validate source
        src_normalized, src_ino = await self._resolve_path_or_throw(src_normalized, "copyfile")
        await assert_readable_existing_inode(self._db, src_ino, "copyfile", src_normalized)

        cursor = await self._db.execute(
            """
            SELECT mode, uid, gid, size FROM fs_inode WHERE ino = ?
            """,
            (src_ino,),
        )
        src_row = await cursor.fetchone()
        if not src_row:
            raise ErrnoException(
                code="ENOENT",
                syscall="copyfile",
                path=src_normalized,
                message="no such file or directory",
            )

        src_mode, src_uid, src_gid, src_size = src_row

        # Destination parent must exist and be a directory
        dest_parent = await self._resolve_parent(dest_normalized)
        if not dest_parent:
            raise ErrnoException(
                code="ENOENT",
                syscall="copyfile",
                path=dest_normalized,
                message="no such file or directory",
            )

        dest_parent_ino, dest_name = dest_parent
        await assert_inode_is_directory(self._db, dest_parent_ino, "copyfile", dest_normalized)

        try:
            now = int(time.time())

            # If destination exists, it must be a file (overwrite semantics)
            dest_ino = await self._resolve_path(dest_normalized)
            if dest_ino is not None:
                dest_mode = await get_inode_mode_or_throw(
                    self._db, dest_ino, "copyfile", dest_normalized
                )
                assert_not_symlink_mode(dest_mode, "copyfile", dest_normalized)
                if (dest_mode & S_IFMT) == S_IFDIR:
                    raise ErrnoException(
                        code="EISDIR",
                        syscall="copyfile",
                        path=dest_normalized,
                        message="illegal operation on a directory",
                    )

                # Replace destination contents
                await self._db.execute("DELETE FROM fs_data WHERE ino = ?", (dest_ino,))
                await self._db.commit()

                # Copy data chunks
                cursor = await self._db.execute(
                    """
                    SELECT chunk_index, data FROM fs_data
                    WHERE ino = ?
                    ORDER BY chunk_index ASC
                    """,
                    (src_ino,),
                )
                src_chunks = await cursor.fetchall()
                for chunk_index, data in src_chunks:
                    await self._db.execute(
                        """
                        INSERT INTO fs_data (ino, chunk_index, data)
                        VALUES (?, ?, ?)
                        """,
                        (dest_ino, chunk_index, data),
                    )

                await self._db.execute(
                    """
                    UPDATE fs_inode
                    SET mode = ?, uid = ?, gid = ?, size = ?, mtime = ?, ctime = ?
                    WHERE ino = ?
                    """,
                    (src_mode, src_uid, src_gid, src_size, now, now, dest_ino),
                )
            else:
                # Create new destination inode + dentry
                dest_ino_created = await self._create_inode(src_mode, src_uid, src_gid)
                await self._create_dentry(dest_parent_ino, dest_name, dest_ino_created)

                # Copy data chunks
                cursor = await self._db.execute(
                    """
                    SELECT chunk_index, data FROM fs_data
                    WHERE ino = ?
                    ORDER BY chunk_index ASC
                    """,
                    (src_ino,),
                )
                src_chunks = await cursor.fetchall()
                for chunk_index, data in src_chunks:
                    await self._db.execute(
                        """
                        INSERT INTO fs_data (ino, chunk_index, data)
                        VALUES (?, ?, ?)
                        """,
                        (dest_ino_created, chunk_index, data),
                    )

                await self._db.execute(
                    """
                    UPDATE fs_inode
                    SET size = ?, mtime = ?, ctime = ?
                    WHERE ino = ?
                    """,
                    (src_size, now, now, dest_ino_created),
                )

            await self._db.commit()
        except Exception:
            raise

    async def access(self, path: str) -> None:
        """Test a user's permissions for the file or directory.
        Currently supports existence checks only (F_OK semantics).

        Args:
            path: Path to check

        Example:
            >>> await fs.access('/data/config.json')
        """
        normalized_path = self._normalize_path(path)
        ino = await self._resolve_path(normalized_path)
        if ino is None:
            raise ErrnoException(
                code="ENOENT",
                syscall="access",
                path=normalized_path,
                message="no such file or directory",
            )
