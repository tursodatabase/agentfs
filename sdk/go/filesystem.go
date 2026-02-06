package agentfs

import (
	"context"
	"database/sql"
	"fmt"
	"path"
	"strings"
	"time"
)

// Filesystem provides POSIX-like file operations backed by SQLite.
type Filesystem struct {
	db        *sql.DB
	chunkSize int
}

// ChunkSize returns the configured chunk size for file data.
func (fs *Filesystem) ChunkSize() int {
	return fs.chunkSize
}

// Stat returns file/directory metadata for the given path.
// If the path refers to a symlink, Stat follows the symlink and returns
// the target's stats. Use Lstat to get the symlink's own stats.
func (fs *Filesystem) Stat(ctx context.Context, p string) (*Stats, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		return nil, err
	}

	return fs.statInode(ctx, ino)
}

// Readdir returns the names of entries in a directory.
func (fs *Filesystem) Readdir(ctx context.Context, p string) ([]string, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		return nil, err
	}

	// Verify it's a directory
	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return nil, err
	}
	if !stats.IsDir() {
		return nil, ErrNotDir("readdir", p)
	}

	rows, err := fs.db.QueryContext(ctx, queryDentriesByParent, ino)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var names []string
	for rows.Next() {
		var name string
		if err := rows.Scan(&name); err != nil {
			return nil, err
		}
		names = append(names, name)
	}

	return names, rows.Err()
}

// ReaddirPlus returns directory entries with their stats (optimized batch operation).
func (fs *Filesystem) ReaddirPlus(ctx context.Context, p string) ([]DirEntry, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		return nil, err
	}

	// Verify it's a directory
	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return nil, err
	}
	if !stats.IsDir() {
		return nil, ErrNotDir("readdir", p)
	}

	rows, err := fs.db.QueryContext(ctx, queryDentriesPlusByParent, ino)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var entries []DirEntry
	for rows.Next() {
		var name string
		var s Stats
		if err := rows.Scan(&name, &s.Ino, &s.Mode, &s.Nlink, &s.UID, &s.GID, &s.Size, &s.Atime, &s.Mtime, &s.Ctime, &s.Rdev,
			&s.AtimeNsec, &s.MtimeNsec, &s.CtimeNsec); err != nil {
			return nil, err
		}
		entries = append(entries, DirEntry{Name: name, Stats: &s})
	}

	return entries, rows.Err()
}

// Mkdir creates a directory.
func (fs *Filesystem) Mkdir(ctx context.Context, p string, mode int64) error {
	p = normalizePath(p)
	if p == "/" {
		return ErrExist("mkdir", p)
	}

	parentPath, name := path.Split(p)
	parentPath = normalizePath(parentPath)

	if err := validateName("mkdir", name); err != nil {
		return err
	}

	parentIno, err := fs.resolvePathFollow(ctx, parentPath, true)
	if err != nil {
		return err
	}

	// Check parent is a directory
	parentStats, err := fs.statInode(ctx, parentIno)
	if err != nil {
		return err
	}
	if !parentStats.IsDir() {
		return ErrNotDir("mkdir", parentPath)
	}

	// Check if entry already exists
	_, err = fs.lookupDentry(ctx, parentIno, name)
	if err == nil {
		return ErrExist("mkdir", p)
	}

	// Create inode
	now := time.Now()
	nowSec := now.Unix()
	nowNsec := int64(now.Nanosecond())
	dirMode := S_IFDIR | (mode & 0o777)

	var ino int64
	err = fs.db.QueryRowContext(ctx, insertInode, dirMode, 0, 0, 0, nowSec, nowSec, nowSec, 0, nowNsec, nowNsec, nowNsec).Scan(&ino)
	if err != nil {
		return err
	}

	// Create dentry
	if _, err := fs.db.ExecContext(ctx, insertDentry, name, parentIno, ino); err != nil {
		return err
	}

	// Increment nlink
	if _, err := fs.db.ExecContext(ctx, incrementNlink, ino); err != nil {
		return err
	}

	return nil
}

// MkdirAll creates a directory and all parent directories as needed.
func (fs *Filesystem) MkdirAll(ctx context.Context, p string, mode int64) error {
	p = normalizePath(p)
	if p == "/" {
		return nil
	}

	components := splitPath(p)
	currentPath := ""

	for _, component := range components {
		currentPath = currentPath + "/" + component

		stats, err := fs.Stat(ctx, currentPath)
		if err != nil {
			if !IsNotExist(err) {
				return err
			}
			// Directory doesn't exist, create it
			if err := fs.Mkdir(ctx, currentPath, mode); err != nil {
				return err
			}
		} else if !stats.IsDir() {
			return ErrNotDir("mkdir", currentPath)
		}
	}

	return nil
}

// ReadFile reads the entire contents of a file.
func (fs *Filesystem) ReadFile(ctx context.Context, p string) ([]byte, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		return nil, err
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return nil, err
	}
	if stats.IsDir() {
		return nil, ErrIsDir("read", p)
	}

	// Read all chunks
	rows, err := fs.db.QueryContext(ctx, queryChunksByIno, ino)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var data []byte
	for rows.Next() {
		var chunk []byte
		if err := rows.Scan(&chunk); err != nil {
			return nil, err
		}
		data = append(data, chunk...)
	}
	if err := rows.Err(); err != nil {
		return nil, err
	}

	// Update atime
	now := time.Now()
	fs.db.ExecContext(ctx, updateInodeAtime, now.Unix(), int64(now.Nanosecond()), ino)

	return data, nil
}

// WriteFile writes data to a file, creating it if it doesn't exist.
func (fs *Filesystem) WriteFile(ctx context.Context, p string, data []byte, mode int64) error {
	p = normalizePath(p)

	parentPath, name := path.Split(p)
	parentPath = normalizePath(parentPath)

	if err := validateName("write", name); err != nil {
		return err
	}

	// Ensure parent directory exists
	if err := fs.MkdirAll(ctx, parentPath, 0o755); err != nil {
		return err
	}

	parentIno, err := fs.resolvePathFollow(ctx, parentPath, true)
	if err != nil {
		return err
	}

	now := time.Now()
	nowSec := now.Unix()
	nowNsec := int64(now.Nanosecond())
	fileMode := S_IFREG | (mode & 0o777)

	// Check if file already exists
	existingIno, err := fs.lookupDentry(ctx, parentIno, name)
	if err == nil {
		// File exists, check it's not a directory
		stats, err := fs.statInode(ctx, existingIno)
		if err != nil {
			return err
		}
		if stats.IsDir() {
			return ErrIsDir("write", p)
		}

		// Delete existing data
		if _, err := fs.db.ExecContext(ctx, deleteChunksByIno, existingIno); err != nil {
			return err
		}

		// Write new data
		if err := fs.writeChunks(ctx, existingIno, data); err != nil {
			return err
		}

		// Update inode
		if _, err := fs.db.ExecContext(ctx, updateInodeSize, len(data), nowSec, nowNsec, existingIno); err != nil {
			return err
		}

		return nil
	}

	// Create new file
	var ino int64
	err = fs.db.QueryRowContext(ctx, insertInode, fileMode, 0, 0, len(data), nowSec, nowSec, nowSec, 0, nowNsec, nowNsec, nowNsec).Scan(&ino)
	if err != nil {
		return err
	}

	// Create dentry
	if _, err := fs.db.ExecContext(ctx, insertDentry, name, parentIno, ino); err != nil {
		return err
	}

	// Increment nlink
	if _, err := fs.db.ExecContext(ctx, incrementNlink, ino); err != nil {
		return err
	}

	// Write data chunks
	if err := fs.writeChunks(ctx, ino, data); err != nil {
		return err
	}

	return nil
}

// Unlink removes a file.
func (fs *Filesystem) Unlink(ctx context.Context, p string) error {
	p = normalizePath(p)
	if p == "/" {
		return ErrRootOperation("unlink", p)
	}

	parentPath, name := path.Split(p)
	parentPath = normalizePath(parentPath)

	parentIno, err := fs.resolvePathFollow(ctx, parentPath, true)
	if err != nil {
		return err
	}

	ino, err := fs.lookupDentry(ctx, parentIno, name)
	if err != nil {
		return ErrNoent("unlink", p)
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return err
	}
	if stats.IsDir() {
		return ErrIsDir("unlink", p)
	}

	// Delete dentry
	if _, err := fs.db.ExecContext(ctx, deleteDentry, parentIno, name); err != nil {
		return err
	}

	// Decrement nlink
	if _, err := fs.db.ExecContext(ctx, decrementNlink, ino); err != nil {
		return err
	}

	// Check if last link
	newStats, err := fs.statInode(ctx, ino)
	if err != nil {
		return err
	}
	if newStats.Nlink == 0 {
		// Delete inode and data
		if _, err := fs.db.ExecContext(ctx, deleteChunksByIno, ino); err != nil {
			return err
		}
		if _, err := fs.db.ExecContext(ctx, deleteSymlink, ino); err != nil {
			return err
		}
		if _, err := fs.db.ExecContext(ctx, deleteInode, ino); err != nil {
			return err
		}
	}

	return nil
}

// Rmdir removes an empty directory.
func (fs *Filesystem) Rmdir(ctx context.Context, p string) error {
	p = normalizePath(p)
	if p == "/" {
		return ErrRootOperation("rmdir", p)
	}

	parentPath, name := path.Split(p)
	parentPath = normalizePath(parentPath)

	parentIno, err := fs.resolvePathFollow(ctx, parentPath, true)
	if err != nil {
		return err
	}

	ino, err := fs.lookupDentry(ctx, parentIno, name)
	if err != nil {
		return ErrNoent("rmdir", p)
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return err
	}
	if !stats.IsDir() {
		return ErrNotDir("rmdir", p)
	}

	// Check if directory is empty
	var count int
	if err := fs.db.QueryRowContext(ctx, countDentriesByParent, ino).Scan(&count); err != nil {
		return err
	}
	if count > 0 {
		return ErrNotEmpty("rmdir", p)
	}

	// Delete dentry
	if _, err := fs.db.ExecContext(ctx, deleteDentry, parentIno, name); err != nil {
		return err
	}

	// Decrement nlink and delete inode
	if _, err := fs.db.ExecContext(ctx, decrementNlink, ino); err != nil {
		return err
	}
	if _, err := fs.db.ExecContext(ctx, deleteInode, ino); err != nil {
		return err
	}

	return nil
}

// Rename moves or renames a file or directory.
func (fs *Filesystem) Rename(ctx context.Context, oldPath, newPath string) error {
	oldPath = normalizePath(oldPath)
	newPath = normalizePath(newPath)

	if oldPath == "/" || newPath == "/" {
		return ErrRootOperation("rename", oldPath)
	}

	oldParentPath, oldName := path.Split(oldPath)
	oldParentPath = normalizePath(oldParentPath)

	newParentPath, newName := path.Split(newPath)
	newParentPath = normalizePath(newParentPath)

	if err := validateName("rename", newName); err != nil {
		return err
	}

	// Prevent renaming a directory into its own subtree
	if strings.HasPrefix(newPath, oldPath+"/") {
		return ErrInvalidRename("rename", oldPath)
	}

	oldParentIno, err := fs.resolvePathFollow(ctx, oldParentPath, true)
	if err != nil {
		return err
	}

	ino, err := fs.lookupDentry(ctx, oldParentIno, oldName)
	if err != nil {
		return ErrNoent("rename", oldPath)
	}

	// Ensure new parent exists
	if err := fs.MkdirAll(ctx, newParentPath, 0o755); err != nil {
		return err
	}

	newParentIno, err := fs.resolvePathFollow(ctx, newParentPath, true)
	if err != nil {
		return err
	}

	// Check if destination exists
	existingIno, err := fs.lookupDentry(ctx, newParentIno, newName)
	if err == nil {
		// Destination exists, remove it
		existingStats, err := fs.statInode(ctx, existingIno)
		if err != nil {
			return err
		}

		sourceStats, err := fs.statInode(ctx, ino)
		if err != nil {
			return err
		}

		if existingStats.IsDir() && !sourceStats.IsDir() {
			return ErrIsDir("rename", newPath)
		}
		if !existingStats.IsDir() && sourceStats.IsDir() {
			return ErrNotDir("rename", newPath)
		}

		if existingStats.IsDir() {
			if err := fs.Rmdir(ctx, newPath); err != nil {
				return err
			}
		} else {
			if err := fs.Unlink(ctx, newPath); err != nil {
				return err
			}
		}
	}

	// Update dentry
	if _, err := fs.db.ExecContext(ctx, updateDentryParent, newParentIno, newName, oldParentIno, oldName); err != nil {
		return err
	}

	return nil
}

// Link creates a hard link.
func (fs *Filesystem) Link(ctx context.Context, existingPath, newPath string) error {
	existingPath = normalizePath(existingPath)
	newPath = normalizePath(newPath)

	_, newName := path.Split(newPath)
	if err := validateName("link", newName); err != nil {
		return err
	}

	ino, err := fs.resolvePathFollow(ctx, existingPath, true)
	if err != nil {
		return err
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return err
	}
	if stats.IsDir() {
		return ErrPerm("link", existingPath)
	}

	newParentPath, newName := path.Split(newPath)
	newParentPath = normalizePath(newParentPath)

	// Ensure parent exists
	if err := fs.MkdirAll(ctx, newParentPath, 0o755); err != nil {
		return err
	}

	newParentIno, err := fs.resolvePathFollow(ctx, newParentPath, true)
	if err != nil {
		return err
	}

	// Check if destination already exists
	_, err = fs.lookupDentry(ctx, newParentIno, newName)
	if err == nil {
		return ErrExist("link", newPath)
	}

	// Create dentry
	if _, err := fs.db.ExecContext(ctx, insertDentry, newName, newParentIno, ino); err != nil {
		return err
	}

	// Increment nlink
	if _, err := fs.db.ExecContext(ctx, incrementNlink, ino); err != nil {
		return err
	}

	return nil
}

// Symlink creates a symbolic link.
func (fs *Filesystem) Symlink(ctx context.Context, target, linkPath string) error {
	linkPath = normalizePath(linkPath)

	parentPath, name := path.Split(linkPath)
	parentPath = normalizePath(parentPath)

	if err := validateName("symlink", name); err != nil {
		return err
	}

	// Ensure parent exists
	if err := fs.MkdirAll(ctx, parentPath, 0o755); err != nil {
		return err
	}

	parentIno, err := fs.resolvePathFollow(ctx, parentPath, true)
	if err != nil {
		return err
	}

	// Check if already exists
	_, err = fs.lookupDentry(ctx, parentIno, name)
	if err == nil {
		return ErrExist("symlink", linkPath)
	}

	// Create inode
	now := time.Now()
	nowSec := now.Unix()
	nowNsec := int64(now.Nanosecond())
	var ino int64
	err = fs.db.QueryRowContext(ctx, insertInode, S_IFLNK|0o777, 0, 0, len(target), nowSec, nowSec, nowSec, 0, nowNsec, nowNsec, nowNsec).Scan(&ino)
	if err != nil {
		return err
	}

	// Create symlink target
	if _, err := fs.db.ExecContext(ctx, insertSymlink, ino, target); err != nil {
		return err
	}

	// Create dentry
	if _, err := fs.db.ExecContext(ctx, insertDentry, name, parentIno, ino); err != nil {
		return err
	}

	// Increment nlink
	if _, err := fs.db.ExecContext(ctx, incrementNlink, ino); err != nil {
		return err
	}

	return nil
}

// Readlink returns the target of a symbolic link.
// Intermediate symlinks in the path are followed, but the final component is not.
func (fs *Filesystem) Readlink(ctx context.Context, p string) (string, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, false)
	if err != nil {
		return "", err
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return "", err
	}
	if !stats.IsSymlink() {
		return "", ErrNotSymlink("readlink", p)
	}

	return fs.readSymlinkTarget(ctx, ino)
}

// Lstat returns file/directory metadata without following the final symlink.
// Intermediate symlinks in the path are still followed.
// If the path refers to a symlink, Lstat returns the symlink's own stats.
func (fs *Filesystem) Lstat(ctx context.Context, p string) (*Stats, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, false)
	if err != nil {
		return nil, err
	}

	return fs.statInode(ctx, ino)
}

// Statfs returns aggregate filesystem statistics.
func (fs *Filesystem) Statfs(ctx context.Context) (*FilesystemStats, error) {
	var stats FilesystemStats

	if err := fs.db.QueryRowContext(ctx, statfsInodeCount).Scan(&stats.Inodes); err != nil {
		return nil, fmt.Errorf("statfs: failed to count inodes: %w", err)
	}

	if err := fs.db.QueryRowContext(ctx, statfsBytesUsed).Scan(&stats.BytesUsed); err != nil {
		return nil, fmt.Errorf("statfs: failed to sum bytes: %w", err)
	}

	return &stats, nil
}

// Chown changes file ownership.
// Pass -1 for uid or gid to leave that field unchanged.
func (fs *Filesystem) Chown(ctx context.Context, p string, uid, gid int64) error {
	if uid == -1 && gid == -1 {
		return nil
	}

	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		return err
	}

	var setClauses []string
	var args []any

	if uid != -1 {
		setClauses = append(setClauses, "uid = ?")
		args = append(args, uid)
	}
	if gid != -1 {
		setClauses = append(setClauses, "gid = ?")
		args = append(args, gid)
	}

	now := time.Now()
	setClauses = append(setClauses, "ctime = ?", "ctime_nsec = ?")
	args = append(args, now.Unix(), int64(now.Nanosecond()))

	args = append(args, ino)
	query := fmt.Sprintf("UPDATE fs_inode SET %s WHERE ino = ?", strings.Join(setClauses, ", "))

	if _, err := fs.db.ExecContext(ctx, query, args...); err != nil {
		return err
	}

	return nil
}

// Mknod creates a special file (FIFO, character device, block device, or socket).
// The mode parameter must include one of S_IFIFO, S_IFCHR, S_IFBLK, or S_IFSOCK.
// The rdev parameter specifies the device number (used for character and block devices).
func (fs *Filesystem) Mknod(ctx context.Context, p string, mode, rdev int64) error {
	p = normalizePath(p)
	if p == "/" {
		return ErrRootOperation("mknod", p)
	}

	parentPath, name := path.Split(p)
	parentPath = normalizePath(parentPath)

	if err := validateName("mknod", name); err != nil {
		return err
	}

	// Validate that mode includes a special file type
	ft := mode & S_IFMT
	if ft != S_IFIFO && ft != S_IFCHR && ft != S_IFBLK && ft != S_IFSOCK {
		return ErrInval("mknod", p, "mode must include S_IFIFO, S_IFCHR, S_IFBLK, or S_IFSOCK")
	}

	parentIno, err := fs.resolvePathFollow(ctx, parentPath, true)
	if err != nil {
		return err
	}

	// Check parent is a directory
	parentStats, err := fs.statInode(ctx, parentIno)
	if err != nil {
		return err
	}
	if !parentStats.IsDir() {
		return ErrNotDir("mknod", parentPath)
	}

	// Check if entry already exists
	_, err = fs.lookupDentry(ctx, parentIno, name)
	if err == nil {
		return ErrExist("mknod", p)
	}

	// Create inode
	now := time.Now()
	nowSec := now.Unix()
	nowNsec := int64(now.Nanosecond())

	var ino int64
	err = fs.db.QueryRowContext(ctx, insertInode, mode, 0, 0, 0, nowSec, nowSec, nowSec, rdev, nowNsec, nowNsec, nowNsec).Scan(&ino)
	if err != nil {
		return err
	}

	// Create dentry
	if _, err := fs.db.ExecContext(ctx, insertDentry, name, parentIno, ino); err != nil {
		return err
	}

	// Increment nlink
	if _, err := fs.db.ExecContext(ctx, incrementNlink, ino); err != nil {
		return err
	}

	return nil
}

// Chmod changes file permissions.
func (fs *Filesystem) Chmod(ctx context.Context, p string, mode int64) error {
	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		return err
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return err
	}

	// Preserve file type, change permissions
	newMode := (stats.Mode & S_IFMT) | (mode & 0o777)
	now := time.Now()

	if _, err := fs.db.ExecContext(ctx, updateInodeMode, newMode, now.Unix(), int64(now.Nanosecond()), ino); err != nil {
		return err
	}

	return nil
}

// Utimes updates file timestamps (seconds precision).
// For nanosecond precision or selective updates, use Utimens.
func (fs *Filesystem) Utimes(ctx context.Context, p string, atime, mtime int64) error {
	return fs.Utimens(ctx, p, TimeSet(atime, 0), TimeSet(mtime, 0))
}

// UtimesNano updates file timestamps with nanosecond precision.
// For selective updates (omit or set to now), use Utimens.
func (fs *Filesystem) UtimesNano(ctx context.Context, p string, atimeSec, atimeNsec, mtimeSec, mtimeNsec int64) error {
	return fs.Utimens(ctx, p, TimeSet(atimeSec, atimeNsec), TimeSet(mtimeSec, mtimeNsec))
}

// Utimens updates file timestamps with nanosecond precision and selective control.
// Each timestamp can be set to a specific value (TimeSet), the current time (TimeNow),
// or left unchanged (TimeOmit).
func (fs *Filesystem) Utimens(ctx context.Context, p string, atime, mtime TimeChange) error {
	if atime.IsOmit() && mtime.IsOmit() {
		return nil
	}

	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		return err
	}

	now := time.Now()

	var setClauses []string
	var args []any

	if !atime.IsOmit() {
		var sec, nsec int64
		if atime.IsNow() {
			sec, nsec = now.Unix(), int64(now.Nanosecond())
		} else {
			sec, nsec = atime.sec, atime.nsec
		}
		setClauses = append(setClauses, "atime = ?", "atime_nsec = ?")
		args = append(args, sec, nsec)
	}

	if !mtime.IsOmit() {
		var sec, nsec int64
		if mtime.IsNow() {
			sec, nsec = now.Unix(), int64(now.Nanosecond())
		} else {
			sec, nsec = mtime.sec, mtime.nsec
		}
		setClauses = append(setClauses, "mtime = ?", "mtime_nsec = ?")
		args = append(args, sec, nsec)
	}

	// Always update ctime
	setClauses = append(setClauses, "ctime = ?", "ctime_nsec = ?")
	args = append(args, now.Unix(), int64(now.Nanosecond()))

	args = append(args, ino)
	query := fmt.Sprintf("UPDATE fs_inode SET %s WHERE ino = ?", strings.Join(setClauses, ", "))

	if _, err := fs.db.ExecContext(ctx, query, args...); err != nil {
		return err
	}

	return nil
}

// Open opens a file and returns a handle for read/write operations.
func (fs *Filesystem) Open(ctx context.Context, p string, flags int) (*File, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		if IsNotExist(err) && (flags&O_CREATE) != 0 {
			// Create the file
			if err := fs.WriteFile(ctx, p, nil, 0o644); err != nil {
				return nil, err
			}
			ino, err = fs.resolvePathFollow(ctx, p, true)
			if err != nil {
				return nil, err
			}
		} else {
			return nil, err
		}
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return nil, err
	}
	if stats.IsDir() {
		return nil, ErrIsDir("open", p)
	}

	if (flags & O_TRUNC) != 0 {
		// Truncate file
		if _, err := fs.db.ExecContext(ctx, deleteChunksByIno, ino); err != nil {
			return nil, err
		}
		now := time.Now()
		if _, err := fs.db.ExecContext(ctx, updateInodeSize, 0, now.Unix(), int64(now.Nanosecond()), ino); err != nil {
			return nil, err
		}
	}

	return &File{
		fs:    fs,
		ino:   ino,
		path:  p,
		flags: flags,
	}, nil
}

// Create creates a new file and returns its stats and a file handle.
func (fs *Filesystem) Create(ctx context.Context, p string, mode int64) (*Stats, *File, error) {
	p = normalizePath(p)

	_, name := path.Split(p)
	if err := validateName("create", name); err != nil {
		return nil, nil, err
	}

	// Create the file (or truncate if exists)
	if err := fs.WriteFile(ctx, p, nil, mode); err != nil {
		return nil, nil, err
	}

	ino, err := fs.resolvePathFollow(ctx, p, true)
	if err != nil {
		return nil, nil, err
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return nil, nil, err
	}

	return stats, &File{
		fs:    fs,
		ino:   ino,
		path:  p,
		flags: O_RDWR,
	}, nil
}

// validateName checks that a filename component is valid.
func validateName(syscall, name string) error {
	if len(name) > MaxNameLen {
		return ErrNameTooLong(syscall, name)
	}
	return nil
}

// Helper functions

// normalizePath cleans and normalizes a path
func normalizePath(p string) string {
	p = path.Clean(p)
	if p == "." || p == "" {
		return "/"
	}
	if !strings.HasPrefix(p, "/") {
		p = "/" + p
	}
	return p
}

// splitPath splits a path into components
func splitPath(p string) []string {
	p = strings.Trim(p, "/")
	if p == "" {
		return nil
	}
	return strings.Split(p, "/")
}

// lookupDentry looks up a directory entry
func (fs *Filesystem) lookupDentry(ctx context.Context, parentIno int64, name string) (int64, error) {
	var ino int64
	err := fs.db.QueryRowContext(ctx, queryDentryByParentAndName, parentIno, name).Scan(&ino)
	if err == sql.ErrNoRows {
		return 0, ErrNoent("lookup", name)
	}
	return ino, err
}

// lookupDentryWithMode looks up a directory entry and returns its inode number and mode
// in a single query (avoids two round-trips per path component).
func (fs *Filesystem) lookupDentryWithMode(ctx context.Context, parentIno int64, name string) (int64, int64, error) {
	var ino, mode int64
	err := fs.db.QueryRowContext(ctx, queryDentryWithMode, parentIno, name).Scan(&ino, &mode)
	if err == sql.ErrNoRows {
		return 0, 0, ErrNoent("lookup", name)
	}
	return ino, mode, err
}

// readSymlinkTarget reads the target of a symlink by inode number.
func (fs *Filesystem) readSymlinkTarget(ctx context.Context, ino int64) (string, error) {
	var target string
	if err := fs.db.QueryRowContext(ctx, querySymlinkTarget, ino).Scan(&target); err != nil {
		return "", err
	}
	return target, nil
}

// resolvePathFollow resolves a path to an inode number, following symlinks.
// If followLast is true, the final component is followed if it is a symlink.
// Returns ELOOP if more than MaxSymlinkDepth symlinks are encountered.
func (fs *Filesystem) resolvePathFollow(ctx context.Context, p string, followLast bool) (int64, error) {
	p = normalizePath(p)

	if p == "/" {
		return RootIno, nil
	}

	symlinkCount := 0

	for {
		components := splitPath(p)
		currentIno := int64(RootIno)
		resolvedDir := "/"

		allResolved := true
		for i, component := range components {
			isLast := i == len(components)-1

			childIno, childMode, err := fs.lookupDentryWithMode(ctx, currentIno, component)
			if err != nil {
				return 0, ErrNoent("resolve", p)
			}

			isSymlink := (childMode & S_IFMT) == S_IFLNK

			if isSymlink && (!isLast || followLast) {
				symlinkCount++
				if symlinkCount > MaxSymlinkDepth {
					return 0, ErrLoop("resolve", p)
				}

				target, err := fs.readSymlinkTarget(ctx, childIno)
				if err != nil {
					return 0, err
				}

				// Build the remaining path after this component
				remaining := strings.Join(components[i+1:], "/")

				if strings.HasPrefix(target, "/") {
					// Absolute symlink target
					if remaining != "" {
						p = target + "/" + remaining
					} else {
						p = target
					}
				} else {
					// Relative symlink target: resolve against parent directory
					if remaining != "" {
						p = resolvedDir + "/" + target + "/" + remaining
					} else {
						p = resolvedDir + "/" + target
					}
				}
				p = normalizePath(p)
				allResolved = false
				break
			}

			if !isLast {
				isDir := (childMode & S_IFMT) == S_IFDIR
				if !isDir && !isSymlink {
					return 0, ErrNotDir("resolve", p)
				}
			}

			currentIno = childIno
			if resolvedDir == "/" {
				resolvedDir = "/" + component
			} else {
				resolvedDir = resolvedDir + "/" + component
			}
		}

		if allResolved {
			return currentIno, nil
		}
	}
}

// statInode retrieves stats for an inode
func (fs *Filesystem) statInode(ctx context.Context, ino int64) (*Stats, error) {
	var s Stats
	err := fs.db.QueryRowContext(ctx, queryInodeByIno, ino).Scan(
		&s.Ino, &s.Mode, &s.Nlink, &s.UID, &s.GID, &s.Size, &s.Atime, &s.Mtime, &s.Ctime, &s.Rdev,
		&s.AtimeNsec, &s.MtimeNsec, &s.CtimeNsec,
	)
	if err == sql.ErrNoRows {
		return nil, ErrNoent("stat", "")
	}
	return &s, err
}

// writeChunks writes data in chunks to fs_data
func (fs *Filesystem) writeChunks(ctx context.Context, ino int64, data []byte) error {
	chunkIndex := 0
	for len(data) > 0 {
		chunkSize := fs.chunkSize
		if len(data) < chunkSize {
			chunkSize = len(data)
		}
		chunk := data[:chunkSize]
		data = data[chunkSize:]

		if _, err := fs.db.ExecContext(ctx, insertChunk, ino, chunkIndex, chunk); err != nil {
			return err
		}
		chunkIndex++
	}
	return nil
}
