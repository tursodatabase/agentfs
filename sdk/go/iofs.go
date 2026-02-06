package agentfs

import (
	"context"
	"io"
	"io/fs"
	"path"
	"sort"
	"time"
)

// IOFS wraps a Filesystem to implement Go's io/fs interfaces.
// It implements: fs.FS, fs.StatFS, fs.ReadFileFS, fs.ReadDirFS, fs.GlobFS, fs.SubFS
//
// Since io/fs interfaces don't support context.Context, use WithContext() to
// set a context for operations, or the default context.Background() is used.
type IOFS struct {
	fs   *Filesystem
	ctx  context.Context
	root string // Subdirectory root (for SubFS), empty means "/"
}

// Compile-time interface compliance checks
var (
	_ fs.FS         = (*IOFS)(nil)
	_ fs.StatFS     = (*IOFS)(nil)
	_ fs.ReadFileFS = (*IOFS)(nil)
	_ fs.ReadDirFS  = (*IOFS)(nil)
	_ fs.SubFS      = (*IOFS)(nil)
	// Note: We intentionally do NOT implement fs.GlobFS.
	// The standard library's fs.Glob will use our ReadDir implementation.
	// Implementing GlobFS by delegating to fs.Glob causes infinite recursion.
)

// NewIOFS creates an io/fs compatible wrapper around a Filesystem.
//
// Example usage:
//
//	afs, _ := agentfs.Open(ctx, agentfs.AgentFSOptions{ID: "my-agent"})
//	iofs := agentfs.NewIOFS(afs.FS)
//
//	// Use with standard library functions
//	fs.WalkDir(iofs, ".", walkFunc)
//	template.ParseFS(iofs, "templates/*.html")
//	http.FileServer(http.FS(iofs))
func NewIOFS(filesystem *Filesystem) *IOFS {
	return &IOFS{
		fs:   filesystem,
		ctx:  context.Background(),
		root: "",
	}
}

// WithContext returns a copy of the IOFS with the given context.
// This context will be used for all filesystem operations.
//
// Example:
//
//	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
//	defer cancel()
//	iofsWithTimeout := iofs.WithContext(ctx)
func (f *IOFS) WithContext(ctx context.Context) *IOFS {
	return &IOFS{
		fs:   f.fs,
		ctx:  ctx,
		root: f.root,
	}
}

// toAbsPath converts an io/fs relative path to an AgentFS absolute path.
// io/fs uses paths like "foo/bar.txt" while AgentFS uses "/foo/bar.txt".
func (f *IOFS) toAbsPath(name string) string {
	if name == "." || name == "" {
		if f.root == "" {
			return "/"
		}
		return f.root
	}
	if f.root == "" {
		return "/" + name
	}
	return f.root + "/" + name
}

// Open implements fs.FS.
// Opens the named file for reading.
func (f *IOFS) Open(name string) (fs.File, error) {
	if !fs.ValidPath(name) {
		return nil, &fs.PathError{Op: "open", Path: name, Err: fs.ErrInvalid}
	}

	absPath := f.toAbsPath(name)
	stats, err := f.fs.Stat(f.ctx, absPath)
	if err != nil {
		if IsNotExist(err) {
			return nil, &fs.PathError{Op: "open", Path: name, Err: fs.ErrNotExist}
		}
		return nil, &fs.PathError{Op: "open", Path: name, Err: err}
	}

	// Handle directories
	if stats.IsDir() {
		return &iofsDir{
			iofs:  f,
			path:  absPath,
			name:  baseName(name),
			stats: stats,
		}, nil
	}

	// Handle regular files
	file, err := f.fs.Open(f.ctx, absPath, O_RDONLY)
	if err != nil {
		return nil, &fs.PathError{Op: "open", Path: name, Err: err}
	}

	return &iofsFile{
		file:   file,
		ctx:    f.ctx,
		name:   baseName(name),
		stats:  stats,
		offset: 0,
	}, nil
}

// Stat implements fs.StatFS.
// Returns a FileInfo describing the named file.
func (f *IOFS) Stat(name string) (fs.FileInfo, error) {
	if !fs.ValidPath(name) {
		return nil, &fs.PathError{Op: "stat", Path: name, Err: fs.ErrInvalid}
	}

	stats, err := f.fs.Stat(f.ctx, f.toAbsPath(name))
	if err != nil {
		if IsNotExist(err) {
			return nil, &fs.PathError{Op: "stat", Path: name, Err: fs.ErrNotExist}
		}
		return nil, &fs.PathError{Op: "stat", Path: name, Err: err}
	}

	return &iofsFileInfo{stats: stats, name: baseName(name)}, nil
}

// ReadFile implements fs.ReadFileFS.
// Reads and returns the entire contents of the named file.
func (f *IOFS) ReadFile(name string) ([]byte, error) {
	if !fs.ValidPath(name) {
		return nil, &fs.PathError{Op: "readfile", Path: name, Err: fs.ErrInvalid}
	}

	data, err := f.fs.ReadFile(f.ctx, f.toAbsPath(name))
	if err != nil {
		if IsNotExist(err) {
			return nil, &fs.PathError{Op: "readfile", Path: name, Err: fs.ErrNotExist}
		}
		return nil, &fs.PathError{Op: "readfile", Path: name, Err: err}
	}

	return data, nil
}

// ReadDir implements fs.ReadDirFS.
// Reads the named directory and returns a list of directory entries sorted by name.
func (f *IOFS) ReadDir(name string) ([]fs.DirEntry, error) {
	if !fs.ValidPath(name) {
		return nil, &fs.PathError{Op: "readdir", Path: name, Err: fs.ErrInvalid}
	}

	entries, err := f.fs.ReaddirPlus(f.ctx, f.toAbsPath(name))
	if err != nil {
		if IsNotExist(err) {
			return nil, &fs.PathError{Op: "readdir", Path: name, Err: fs.ErrNotExist}
		}
		var fsErr *FSError
		if ok := errorAs(err, &fsErr); ok && fsErr.Code == ENOTDIR {
			return nil, &fs.PathError{Op: "readdir", Path: name, Err: fs.ErrInvalid}
		}
		return nil, &fs.PathError{Op: "readdir", Path: name, Err: err}
	}

	result := make([]fs.DirEntry, len(entries))
	for i := range entries {
		result[i] = &iofsDirEntry{entry: &entries[i]}
	}

	// Sort by name (should already be sorted, but ensure it)
	sort.Slice(result, func(i, j int) bool {
		return result[i].Name() < result[j].Name()
	})

	return result, nil
}

// Sub implements fs.SubFS.
// Returns an FS corresponding to the subtree rooted at dir.
func (f *IOFS) Sub(dir string) (fs.FS, error) {
	if !fs.ValidPath(dir) {
		return nil, &fs.PathError{Op: "sub", Path: dir, Err: fs.ErrInvalid}
	}

	if dir == "." {
		return f, nil
	}

	absDir := f.toAbsPath(dir)
	stats, err := f.fs.Stat(f.ctx, absDir)
	if err != nil {
		if IsNotExist(err) {
			return nil, &fs.PathError{Op: "sub", Path: dir, Err: fs.ErrNotExist}
		}
		return nil, &fs.PathError{Op: "sub", Path: dir, Err: err}
	}

	if !stats.IsDir() {
		return nil, &fs.PathError{Op: "sub", Path: dir, Err: fs.ErrInvalid}
	}

	return &IOFS{
		fs:   f.fs,
		ctx:  f.ctx,
		root: absDir,
	}, nil
}

// baseName returns the base name of a path, handling "." specially
func baseName(name string) string {
	if name == "." || name == "" {
		return "."
	}
	return path.Base(name)
}

// errorAs is a helper to avoid import cycle with errors package
func errorAs(err error, target interface{}) bool {
	if err == nil {
		return false
	}
	if fsErr, ok := target.(**FSError); ok {
		if e, ok := err.(*FSError); ok {
			*fsErr = e
			return true
		}
	}
	return false
}

// ============================================================================
// iofsFile - implements fs.File for regular files
// ============================================================================

// iofsFile wraps File to implement fs.File with stateful reading.
type iofsFile struct {
	file   *File
	ctx    context.Context
	name   string
	stats  *Stats
	offset int64
}

// Stat implements fs.File.
func (f *iofsFile) Stat() (fs.FileInfo, error) {
	// Refresh stats
	stats, err := f.file.Stat(f.ctx)
	if err != nil {
		return nil, err
	}
	return &iofsFileInfo{stats: stats, name: f.name}, nil
}

// Read implements fs.File.
// Reads up to len(b) bytes into b, advancing the file offset.
func (f *iofsFile) Read(b []byte) (int, error) {
	if len(b) == 0 {
		return 0, nil
	}

	n, err := f.file.Pread(f.ctx, b, f.offset)
	f.offset += int64(n)

	// Convert zero-read to EOF
	if n == 0 && err == nil {
		return 0, io.EOF
	}

	return n, err
}

// Close implements fs.File.
func (f *iofsFile) Close() error {
	return f.file.Close()
}

// ============================================================================
// iofsDir - implements fs.File and fs.ReadDirFile for directories
// ============================================================================

// iofsDir implements fs.File and fs.ReadDirFile for directories.
type iofsDir struct {
	iofs    *IOFS
	path    string
	name    string
	stats   *Stats
	entries []fs.DirEntry // Lazy-loaded
	offset  int           // Current position for ReadDir iteration
}

// Compile-time check that iofsDir implements fs.ReadDirFile
var _ fs.ReadDirFile = (*iofsDir)(nil)

// Stat implements fs.File.
func (d *iofsDir) Stat() (fs.FileInfo, error) {
	return &iofsFileInfo{stats: d.stats, name: d.name}, nil
}

// Read implements fs.File.
// Reading from a directory is not permitted.
func (d *iofsDir) Read(b []byte) (int, error) {
	return 0, &fs.PathError{Op: "read", Path: d.name, Err: fs.ErrInvalid}
}

// Close implements fs.File.
func (d *iofsDir) Close() error {
	d.entries = nil
	d.offset = 0
	return nil
}

// ReadDir implements fs.ReadDirFile.
// Reads the contents of the directory and returns up to n DirEntry values.
// If n <= 0, ReadDir returns all entries.
func (d *iofsDir) ReadDir(n int) ([]fs.DirEntry, error) {
	// Lazy-load entries on first call
	if d.entries == nil {
		entries, err := d.iofs.fs.ReaddirPlus(d.iofs.ctx, d.path)
		if err != nil {
			return nil, err
		}

		d.entries = make([]fs.DirEntry, len(entries))
		for i := range entries {
			d.entries[i] = &iofsDirEntry{entry: &entries[i]}
		}

		// Sort by name
		sort.Slice(d.entries, func(i, j int) bool {
			return d.entries[i].Name() < d.entries[j].Name()
		})
	}

	// Return all remaining entries
	if n <= 0 {
		if d.offset >= len(d.entries) {
			return nil, nil
		}
		entries := d.entries[d.offset:]
		d.offset = len(d.entries)
		return entries, nil
	}

	// Return up to n entries
	if d.offset >= len(d.entries) {
		return nil, io.EOF
	}

	end := d.offset + n
	if end > len(d.entries) {
		end = len(d.entries)
	}

	entries := d.entries[d.offset:end]
	d.offset = end

	return entries, nil
}

// ============================================================================
// iofsFileInfo - implements fs.FileInfo
// ============================================================================

// iofsFileInfo adapts *Stats to implement fs.FileInfo.
type iofsFileInfo struct {
	stats *Stats
	name  string
}

// Compile-time check
var _ fs.FileInfo = (*iofsFileInfo)(nil)

// Name returns the base name of the file.
func (fi *iofsFileInfo) Name() string {
	return fi.name
}

// Size returns the file size in bytes.
func (fi *iofsFileInfo) Size() int64 {
	return fi.stats.Size
}

// Mode returns the file mode bits.
func (fi *iofsFileInfo) Mode() fs.FileMode {
	return statsToFileMode(fi.stats)
}

// ModTime returns the modification time.
func (fi *iofsFileInfo) ModTime() time.Time {
	return time.Unix(fi.stats.Mtime, 0)
}

// IsDir reports whether the file is a directory.
func (fi *iofsFileInfo) IsDir() bool {
	return fi.stats.IsDir()
}

// Sys returns the underlying *Stats.
func (fi *iofsFileInfo) Sys() any {
	return fi.stats
}

// ============================================================================
// iofsDirEntry - implements fs.DirEntry
// ============================================================================

// iofsDirEntry adapts *DirEntry to implement fs.DirEntry.
type iofsDirEntry struct {
	entry *DirEntry
}

// Compile-time check
var _ fs.DirEntry = (*iofsDirEntry)(nil)

// Name returns the name of the file or directory.
func (de *iofsDirEntry) Name() string {
	return de.entry.Name
}

// IsDir reports whether the entry describes a directory.
func (de *iofsDirEntry) IsDir() bool {
	return de.entry.Stats.IsDir()
}

// Type returns the type bits for the entry (ModeDir, ModeSymlink, etc.).
func (de *iofsDirEntry) Type() fs.FileMode {
	return statsToFileMode(de.entry.Stats).Type()
}

// Info returns the FileInfo for the file or directory.
func (de *iofsDirEntry) Info() (fs.FileInfo, error) {
	return &iofsFileInfo{stats: de.entry.Stats, name: de.entry.Name}, nil
}

// ============================================================================
// Helper functions
// ============================================================================

// statsToFileMode converts Stats.Mode to fs.FileMode.
func statsToFileMode(s *Stats) fs.FileMode {
	mode := fs.FileMode(s.Mode & 0o777) // Permission bits

	// File type
	switch s.Mode & S_IFMT {
	case S_IFDIR:
		mode |= fs.ModeDir
	case S_IFLNK:
		mode |= fs.ModeSymlink
	case S_IFIFO:
		mode |= fs.ModeNamedPipe
	case S_IFSOCK:
		mode |= fs.ModeSocket
	case S_IFCHR:
		mode |= fs.ModeDevice | fs.ModeCharDevice
	case S_IFBLK:
		mode |= fs.ModeDevice
	}

	return mode
}
