"""Unix-like filesystem with inodes and directory entries."""
from dataclasses import dataclass
from typing import List, TYPE_CHECKING

if TYPE_CHECKING:
    from .turso_async import AsyncTursoConnection


# File type constants
S_IFMT = 0o170000
S_IFREG = 0o100000  # Regular file
S_IFDIR = 0o040000  # Directory
S_IFLNK = 0o120000  # Symbolic link


@dataclass
class Stats:
    """File statistics."""
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
        return (self.mode & S_IFMT) == S_IFREG

    def is_directory(self) -> bool:
        return (self.mode & S_IFMT) == S_IFDIR

    def is_symbolic_link(self) -> bool:
        return (self.mode & S_IFMT) == S_IFLNK


class Filesystem:
    """Async filesystem backed by SQLite."""

    def __init__(self, db: "AsyncTursoConnection"):
        self._db = db
        self._ready = False

    async def ready(self) -> None:
        """Initialize database schema."""
        if self._ready:
            return

        # Enable foreign key constraints
        await self._db.execute("PRAGMA foreign_keys = ON")

        # Create tables
        await self._db.execute("""
            CREATE TABLE IF NOT EXISTS fs_inode (
                ino INTEGER PRIMARY KEY AUTOINCREMENT,
                mode INTEGER NOT NULL,
                uid INTEGER NOT NULL DEFAULT 0,
                gid INTEGER NOT NULL DEFAULT 0,
                size INTEGER NOT NULL DEFAULT 0,
                atime INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                ctime INTEGER NOT NULL
            )
        """)

        await self._db.execute("""
            CREATE TABLE IF NOT EXISTS fs_dentry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                parent_ino INTEGER NOT NULL,
                ino INTEGER NOT NULL,
                FOREIGN KEY (ino) REFERENCES fs_inode(ino),
                UNIQUE(parent_ino, name)
            )
        """)

        await self._db.execute("""
            CREATE TABLE IF NOT EXISTS fs_data (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ino INTEGER NOT NULL,
                offset INTEGER NOT NULL,
                size INTEGER NOT NULL,
                data BLOB NOT NULL,
                FOREIGN KEY (ino) REFERENCES fs_inode(ino)
            )
        """)

        await self._db.execute("""
            CREATE TABLE IF NOT EXISTS fs_symlink (
                ino INTEGER PRIMARY KEY,
                target TEXT NOT NULL,
                FOREIGN KEY (ino) REFERENCES fs_inode(ino)
            )
        """)

        await self._db.commit()

        # Create root directory if it doesn't exist
        cursor = await self._db.execute(
            "SELECT ino FROM fs_inode WHERE ino = 1"
        )
        if await cursor.fetchone() is None:
            import time
            now = int(time.time())
            await self._db.execute("""
                INSERT INTO fs_inode (ino, mode, uid, gid, size, atime, mtime, ctime)
                VALUES (1, ?, 0, 0, 0, ?, ?, ?)
            """, (S_IFDIR | 0o755, now, now, now))
            await self._db.commit()

        self._ready = True

    async def _resolve_path(self, path: str) -> int:
        """Resolve path to inode number."""
        path = path.rstrip('/')
        if not path:
            path = '/'

        if path == '/':
            return 1

        parts = [p for p in path.split('/') if p]
        current_ino = 1

        for part in parts:
            cursor = await self._db.execute("""
                SELECT ino FROM fs_dentry
                WHERE parent_ino = ? AND name = ?
            """, (current_ino, part))
            row = await cursor.fetchone()
            if row is None:
                raise FileNotFoundError(f"No such file or directory: {path}")
            current_ino = row[0]

        return current_ino

    async def _create_inode(self, mode: int) -> int:
        """Create new inode."""
        import time
        now = int(time.time())
        cursor = await self._db.execute("""
            INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
            VALUES (?, 0, 0, 0, ?, ?, ?)
        """, (mode, now, now, now))
        await self._db.commit()
        return cursor.lastrowid

    async def _ensure_directory(self, path: str) -> int:
        """Ensure directory exists, creating if needed."""
        try:
            return await self._resolve_path(path)
        except FileNotFoundError:
            parent_path = '/'.join(path.rstrip('/').split('/')[:-1]) or '/'
            parent_ino = await self._ensure_directory(parent_path)
            name = path.rstrip('/').split('/')[-1]
            ino = await self._create_inode(S_IFDIR | 0o755)
            await self._db.execute("""
                INSERT INTO fs_dentry (name, parent_ino, ino)
                VALUES (?, ?, ?)
            """, (name, parent_ino, ino))
            await self._db.commit()
            return ino

    async def write_file(self, path: str, content: str | bytes) -> None:
        """Write file, creating parent directories as needed."""
        if isinstance(content, str):
            content = content.encode('utf-8')

        parent_path = '/'.join(path.rstrip('/').split('/')[:-1]) or '/'
        parent_ino = await self._ensure_directory(parent_path)
        name = path.split('/')[-1]

        # Check if file exists
        try:
            ino = await self._resolve_path(path)
            # Delete old data
            await self._db.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
        except FileNotFoundError:
            # Create new file
            ino = await self._create_inode(S_IFREG | 0o644)
            await self._db.execute("""
                INSERT INTO fs_dentry (name, parent_ino, ino)
                VALUES (?, ?, ?)
            """, (name, parent_ino, ino))

        # Write data
        await self._db.execute("""
            INSERT INTO fs_data (ino, offset, size, data)
            VALUES (?, 0, ?, ?)
        """, (ino, len(content), content))

        # Update inode
        import time
        await self._db.execute("""
            UPDATE fs_inode SET size = ?, mtime = ?
            WHERE ino = ?
        """, (len(content), int(time.time()), ino))
        await self._db.commit()

    async def read_file(self, path: str) -> str:
        """Read file content."""
        ino = await self._resolve_path(path)
        cursor = await self._db.execute("""
            SELECT data FROM fs_data WHERE ino = ? ORDER BY offset
        """, (ino,))
        rows = await cursor.fetchall()
        if not rows:
            return ''
        return b''.join(row[0] for row in rows).decode('utf-8')

    async def readdir(self, path: str) -> List[str]:
        """List directory contents."""
        ino = await self._resolve_path(path)
        cursor = await self._db.execute("""
            SELECT name FROM fs_dentry WHERE parent_ino = ?
        """, (ino,))
        rows = await cursor.fetchall()
        return [row[0] for row in rows]

    async def delete_file(self, path: str) -> None:
        """Delete file or directory."""
        ino = await self._resolve_path(path)
        # Manual cascade since Turso doesnt support ON DELETE CASCADE
        await self._db.execute("DELETE FROM fs_data WHERE ino = ?", (ino,))
        await self._db.execute("DELETE FROM fs_symlink WHERE ino = ?", (ino,))
        await self._db.execute("DELETE FROM fs_dentry WHERE ino = ?", (ino,))
        await self._db.execute("DELETE FROM fs_inode WHERE ino = ?", (ino,))
        await self._db.commit()

    async def stat(self, path: str) -> Stats:
        """Get file statistics."""
        ino = await self._resolve_path(path)
        cursor = await self._db.execute("""
            SELECT ino, mode, uid, gid, size, atime, mtime, ctime
            FROM fs_inode WHERE ino = ?
        """, (ino,))
        row = await cursor.fetchone()
        if row is None:
            raise FileNotFoundError(path)

        # Count links
        link_cursor = await self._db.execute("""
            SELECT COUNT(*) FROM fs_dentry WHERE ino = ?
        """, (ino,))
        nlink = (await link_cursor.fetchone())[0]

        return Stats(
            ino=row[0],
            mode=row[1],
            nlink=nlink,
            uid=row[2],
            gid=row[3],
            size=row[4],
            atime=row[5],
            mtime=row[6],
            ctime=row[7]
        )