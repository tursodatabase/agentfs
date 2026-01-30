package agentfs

import (
	"context"
	"database/sql"
	"path"
	"strings"
	"time"

	"github.com/tursodatabase/agentfs/sdk/go/internal/cache"
)

// Filesystem provides POSIX-like file operations backed by SQLite.
type Filesystem struct {
	db        *sql.DB
	chunkSize int
	cache     cache.PathCache // nil if caching disabled
}

// ChunkSize returns the configured chunk size for file data.
func (fs *Filesystem) ChunkSize() int {
	return fs.chunkSize
}

// Stat returns file/directory metadata for the given path.
func (fs *Filesystem) Stat(ctx context.Context, p string) (*Stats, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePath(ctx, p)
	if err != nil {
		return nil, err
	}

	return fs.statInode(ctx, ino)
}

// Readdir returns the names of entries in a directory.
func (fs *Filesystem) Readdir(ctx context.Context, p string) ([]string, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePath(ctx, p)
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

	ino, err := fs.resolvePath(ctx, p)
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

	parentIno, err := fs.resolvePath(ctx, parentPath)
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

	ino, err := fs.resolvePath(ctx, p)
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

	// Ensure parent directory exists
	if err := fs.MkdirAll(ctx, parentPath, 0o755); err != nil {
		return err
	}

	parentIno, err := fs.resolvePath(ctx, parentPath)
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
		return ErrPerm("unlink", p)
	}

	parentPath, name := path.Split(p)
	parentPath = normalizePath(parentPath)

	parentIno, err := fs.resolvePath(ctx, parentPath)
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

	// Invalidate cache
	if fs.cache != nil {
		fs.cache.Delete(p)
	}

	return nil
}

// Rmdir removes an empty directory.
func (fs *Filesystem) Rmdir(ctx context.Context, p string) error {
	p = normalizePath(p)
	if p == "/" {
		return ErrPerm("rmdir", p)
	}

	parentPath, name := path.Split(p)
	parentPath = normalizePath(parentPath)

	parentIno, err := fs.resolvePath(ctx, parentPath)
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

	// Invalidate cache (directory and any cached children paths)
	if fs.cache != nil {
		fs.cache.Delete(p)
		fs.cache.DeletePrefix(p + "/")
	}

	return nil
}

// Rename moves or renames a file or directory.
func (fs *Filesystem) Rename(ctx context.Context, oldPath, newPath string) error {
	oldPath = normalizePath(oldPath)
	newPath = normalizePath(newPath)

	if oldPath == "/" || newPath == "/" {
		return ErrPerm("rename", oldPath)
	}

	oldParentPath, oldName := path.Split(oldPath)
	oldParentPath = normalizePath(oldParentPath)

	newParentPath, newName := path.Split(newPath)
	newParentPath = normalizePath(newParentPath)

	oldParentIno, err := fs.resolvePath(ctx, oldParentPath)
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

	newParentIno, err := fs.resolvePath(ctx, newParentPath)
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

	// Invalidate cache for both old and new paths (and children if directory)
	if fs.cache != nil {
		fs.cache.Delete(oldPath)
		fs.cache.DeletePrefix(oldPath + "/")
		fs.cache.Delete(newPath)
		fs.cache.DeletePrefix(newPath + "/")
	}

	return nil
}

// Link creates a hard link.
func (fs *Filesystem) Link(ctx context.Context, existingPath, newPath string) error {
	existingPath = normalizePath(existingPath)
	newPath = normalizePath(newPath)

	ino, err := fs.resolvePath(ctx, existingPath)
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

	newParentIno, err := fs.resolvePath(ctx, newParentPath)
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

	// Ensure parent exists
	if err := fs.MkdirAll(ctx, parentPath, 0o755); err != nil {
		return err
	}

	parentIno, err := fs.resolvePath(ctx, parentPath)
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
func (fs *Filesystem) Readlink(ctx context.Context, p string) (string, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePath(ctx, p)
	if err != nil {
		return "", err
	}

	stats, err := fs.statInode(ctx, ino)
	if err != nil {
		return "", err
	}
	if !stats.IsSymlink() {
		return "", ErrInval("readlink", p, "not a symlink")
	}

	var target string
	if err := fs.db.QueryRowContext(ctx, querySymlinkTarget, ino).Scan(&target); err != nil {
		return "", err
	}

	return target, nil
}

// Chmod changes file permissions.
func (fs *Filesystem) Chmod(ctx context.Context, p string, mode int64) error {
	p = normalizePath(p)

	ino, err := fs.resolvePath(ctx, p)
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
// For nanosecond precision, use UtimesNano.
func (fs *Filesystem) Utimes(ctx context.Context, p string, atime, mtime int64) error {
	return fs.UtimesNano(ctx, p, atime, 0, mtime, 0)
}

// UtimesNano updates file timestamps with nanosecond precision.
func (fs *Filesystem) UtimesNano(ctx context.Context, p string, atimeSec, atimeNsec, mtimeSec, mtimeNsec int64) error {
	p = normalizePath(p)

	ino, err := fs.resolvePath(ctx, p)
	if err != nil {
		return err
	}

	now := time.Now()
	if _, err := fs.db.ExecContext(ctx, updateInodeTimes, atimeSec, mtimeSec, now.Unix(), atimeNsec, mtimeNsec, int64(now.Nanosecond()), ino); err != nil {
		return err
	}

	return nil
}

// Open opens a file and returns a handle for read/write operations.
func (fs *Filesystem) Open(ctx context.Context, p string, flags int) (*File, error) {
	p = normalizePath(p)

	ino, err := fs.resolvePath(ctx, p)
	if err != nil {
		if IsNotExist(err) && (flags&O_CREATE) != 0 {
			// Create the file
			if err := fs.WriteFile(ctx, p, nil, 0o644); err != nil {
				return nil, err
			}
			ino, err = fs.resolvePath(ctx, p)
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

// Create creates a new file and returns a handle.
func (fs *Filesystem) Create(ctx context.Context, p string, mode int64) (*File, error) {
	p = normalizePath(p)

	// Create the file (or truncate if exists)
	if err := fs.WriteFile(ctx, p, nil, mode); err != nil {
		return nil, err
	}

	ino, err := fs.resolvePath(ctx, p)
	if err != nil {
		return nil, err
	}

	return &File{
		fs:    fs,
		ino:   ino,
		path:  p,
		flags: O_RDWR,
	}, nil
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

// resolvePath resolves a path to an inode number
func (fs *Filesystem) resolvePath(ctx context.Context, p string) (int64, error) {
	p = normalizePath(p)

	if p == "/" {
		return RootIno, nil
	}

	// Check cache first
	if fs.cache != nil {
		if ino, ok := fs.cache.Get(p); ok {
			return ino, nil
		}
	}

	// Cache miss - do database lookup
	components := splitPath(p)
	currentIno := int64(RootIno)

	for _, component := range components {
		ino, err := fs.lookupDentry(ctx, currentIno, component)
		if err != nil {
			return 0, ErrNoent("resolve", p)
		}
		currentIno = ino
	}

	// Cache the result
	if fs.cache != nil {
		fs.cache.Set(p, currentIno)
	}

	return currentIno, nil
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

// CacheStats returns cache statistics, or nil if caching is disabled.
func (fs *Filesystem) CacheStats() *cache.Stats {
	if fs.cache == nil {
		return nil
	}
	stats := fs.cache.Stats()
	return &stats
}

// ClearCache clears all cached path resolutions.
// This is useful when external changes may have occurred.
func (fs *Filesystem) ClearCache() {
	if fs.cache != nil {
		fs.cache.Clear()
	}
}
