"""Guard functions for filesystem operations validation"""

from typing import Any, Dict, Optional

from turso.aio import Connection

from .constants import S_IFDIR, S_IFLNK, S_IFMT
from .errors import ErrnoException, FsSyscall


async def _get_inode_mode(db: Connection, ino: int) -> Optional[int]:
    """Get mode for an inode"""
    cursor = await db.execute("SELECT mode FROM fs_inode WHERE ino = ?", (ino,))
    row = await cursor.fetchone()
    return row[0] if row else None


def _is_dir_mode(mode: int) -> bool:
    """Check if mode represents a directory"""
    return (mode & S_IFMT) == S_IFDIR


async def get_inode_mode_or_throw(
    db: Connection,
    ino: int,
    syscall: FsSyscall,
    path: str,
) -> int:
    """Get inode mode or throw ENOENT if not found"""
    mode = await _get_inode_mode(db, ino)
    if mode is None:
        raise ErrnoException(
            code="ENOENT",
            syscall=syscall,
            path=path,
            message="no such file or directory",
        )
    return mode


def assert_not_root(path: str, syscall: FsSyscall) -> None:
    """Assert that path is not root directory"""
    if path == "/":
        raise ErrnoException(
            code="EPERM",
            syscall=syscall,
            path=path,
            message="operation not permitted on root directory",
        )


def normalize_rm_options(options: Optional[Dict[str, Any]]) -> Dict[str, bool]:
    """Normalize rm options to ensure force and recursive are booleans"""
    return {
        "force": options.get("force", False) if options else False,
        "recursive": options.get("recursive", False) if options else False,
    }


def throw_enoent_unless_force(path: str, syscall: FsSyscall, force: bool) -> None:
    """Throw ENOENT unless force flag is set"""
    if force:
        return
    raise ErrnoException(
        code="ENOENT",
        syscall=syscall,
        path=path,
        message="no such file or directory",
    )


def assert_not_symlink_mode(mode: int, syscall: FsSyscall, path: str) -> None:
    """Assert that mode does not represent a symlink"""
    if (mode & S_IFMT) == S_IFLNK:
        raise ErrnoException(
            code="ENOSYS",
            syscall=syscall,
            path=path,
            message="symbolic links not supported yet",
        )


async def _assert_existing_non_dir_non_symlink_inode(
    db: Connection,
    ino: int,
    syscall: FsSyscall,
    full_path_for_error: str,
) -> None:
    """Assert inode exists and is neither directory nor symlink"""
    mode = await _get_inode_mode(db, ino)
    if mode is None:
        raise ErrnoException(
            code="ENOENT",
            syscall=syscall,
            path=full_path_for_error,
            message="no such file or directory",
        )
    if _is_dir_mode(mode):
        raise ErrnoException(
            code="EISDIR",
            syscall=syscall,
            path=full_path_for_error,
            message="illegal operation on a directory",
        )
    assert_not_symlink_mode(mode, syscall, full_path_for_error)


async def assert_inode_is_directory(
    db: Connection,
    ino: int,
    syscall: FsSyscall,
    full_path_for_error: str,
) -> None:
    """Assert that inode is a directory"""
    mode = await _get_inode_mode(db, ino)
    if mode is None:
        raise ErrnoException(
            code="ENOENT",
            syscall=syscall,
            path=full_path_for_error,
            message="no such file or directory",
        )
    if not _is_dir_mode(mode):
        raise ErrnoException(
            code="ENOTDIR",
            syscall=syscall,
            path=full_path_for_error,
            message="not a directory",
        )


async def assert_writable_existing_inode(
    db: Connection,
    ino: int,
    syscall: FsSyscall,
    full_path_for_error: str,
) -> None:
    """Assert inode is writable (exists and is not directory/symlink)"""
    await _assert_existing_non_dir_non_symlink_inode(db, ino, syscall, full_path_for_error)


async def assert_readable_existing_inode(
    db: Connection,
    ino: int,
    syscall: FsSyscall,
    full_path_for_error: str,
) -> None:
    """Assert inode is readable (exists and is not directory/symlink)"""
    await _assert_existing_non_dir_non_symlink_inode(db, ino, syscall, full_path_for_error)


async def assert_readdir_target_inode(
    db: Connection,
    ino: int,
    full_path_for_error: str,
) -> None:
    """Assert inode is a valid readdir target (directory, not symlink)"""
    syscall: FsSyscall = "scandir"
    mode = await _get_inode_mode(db, ino)
    if mode is None:
        raise ErrnoException(
            code="ENOENT",
            syscall=syscall,
            path=full_path_for_error,
            message="no such file or directory",
        )
    assert_not_symlink_mode(mode, syscall, full_path_for_error)
    if not _is_dir_mode(mode):
        raise ErrnoException(
            code="ENOTDIR",
            syscall=syscall,
            path=full_path_for_error,
            message="not a directory",
        )


async def assert_unlink_target_inode(
    db: Connection,
    ino: int,
    full_path_for_error: str,
) -> None:
    """Assert inode is a valid unlink target (file, not directory/symlink)"""
    syscall: FsSyscall = "unlink"
    mode = await _get_inode_mode(db, ino)
    if mode is None:
        raise ErrnoException(
            code="ENOENT",
            syscall=syscall,
            path=full_path_for_error,
            message="no such file or directory",
        )
    if _is_dir_mode(mode):
        raise ErrnoException(
            code="EISDIR",
            syscall=syscall,
            path=full_path_for_error,
            message="illegal operation on a directory",
        )
    assert_not_symlink_mode(mode, syscall, full_path_for_error)
