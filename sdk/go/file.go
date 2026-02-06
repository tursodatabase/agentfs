package agentfs

import (
	"context"
	"io"
	"time"
)

// File represents an open file handle for read/write operations.
//
// File implements the following io interfaces:
//   - io.Reader, io.Writer (sequential I/O with position tracking)
//   - io.ReaderAt, io.WriterAt (random access I/O)
//   - io.Seeker (position control)
//   - io.Closer
//
// For context-aware operations, use Pread/Pwrite or set the context via WithContext.
type File struct {
	fs     *Filesystem
	ino    int64
	path   string
	flags  int
	offset int64           // Current file position for Read/Write
	ctx    context.Context // Context for streaming operations
}

// Compile-time interface checks
var (
	_ io.Reader      = (*File)(nil)
	_ io.Writer      = (*File)(nil)
	_ io.Seeker      = (*File)(nil)
	_ io.Closer      = (*File)(nil)
	_ io.ReaderAt    = (*File)(nil)
	_ io.WriterAt    = (*File)(nil)
	_ io.ReadSeeker  = (*File)(nil)
	_ io.WriteSeeker = (*File)(nil)
)

// WithContext returns a copy of the File with the given context.
// This context will be used for Read, Write, and Seek operations.
func (f *File) WithContext(ctx context.Context) *File {
	return &File{
		fs:     f.fs,
		ino:    f.ino,
		path:   f.path,
		flags:  f.flags,
		offset: f.offset,
		ctx:    ctx,
	}
}

// context returns the file's context, defaulting to Background if not set.
func (f *File) context() context.Context {
	if f.ctx != nil {
		return f.ctx
	}
	return context.Background()
}

// Read reads up to len(p) bytes into p, advancing the file offset.
// It implements io.Reader.
//
// Read returns io.EOF when the end of file is reached.
func (f *File) Read(p []byte) (int, error) {
	n, err := f.Pread(f.context(), p, f.offset)
	f.offset += int64(n)

	// Convert zero-read at EOF to io.EOF
	if n == 0 && err == nil {
		return 0, io.EOF
	}
	return n, err
}

// Write writes len(p) bytes from p, advancing the file offset.
// It implements io.Writer.
func (f *File) Write(p []byte) (int, error) {
	n, err := f.Pwrite(f.context(), p, f.offset)
	f.offset += int64(n)
	return n, err
}

// Seek sets the offset for the next Read or Write.
// It implements io.Seeker.
//
// Whence values:
//   - io.SeekStart (0): offset is relative to the start of the file
//   - io.SeekCurrent (1): offset is relative to the current position
//   - io.SeekEnd (2): offset is relative to the end of the file
func (f *File) Seek(offset int64, whence int) (int64, error) {
	var newOffset int64

	switch whence {
	case io.SeekStart:
		newOffset = offset
	case io.SeekCurrent:
		newOffset = f.offset + offset
	case io.SeekEnd:
		stats, err := f.fs.statInode(f.context(), f.ino)
		if err != nil {
			return 0, err
		}
		newOffset = stats.Size + offset
	default:
		return 0, ErrInval("seek", f.path, "invalid whence")
	}

	if newOffset < 0 {
		return 0, ErrInval("seek", f.path, "negative offset")
	}

	f.offset = newOffset
	return newOffset, nil
}

// ReadAt reads len(p) bytes at the given offset.
// It implements io.ReaderAt.
//
// ReadAt does not affect the file's current offset.
func (f *File) ReadAt(p []byte, off int64) (int, error) {
	n, err := f.Pread(f.context(), p, off)

	// io.ReaderAt requires returning io.EOF if fewer bytes were read
	if n < len(p) && err == nil {
		err = io.EOF
	}
	return n, err
}

// WriteAt writes len(p) bytes at the given offset.
// It implements io.WriterAt.
//
// WriteAt does not affect the file's current offset.
func (f *File) WriteAt(p []byte, off int64) (int, error) {
	return f.Pwrite(f.context(), p, off)
}

// Offset returns the current file offset.
func (f *File) Offset() int64 {
	return f.offset
}

// Pread reads data at a specific offset (positioned read).
// Returns the number of bytes read and any error.
//
// Unlike Read, Pread does not modify the file's current offset.
func (f *File) Pread(ctx context.Context, buf []byte, offset int64) (int, error) {
	if len(buf) == 0 {
		return 0, nil
	}

	stats, err := f.fs.statInode(ctx, f.ino)
	if err != nil {
		return 0, err
	}

	// Check bounds
	if offset >= stats.Size {
		return 0, nil
	}

	length := int64(len(buf))
	if offset+length > stats.Size {
		length = stats.Size - offset
	}

	chunkSize := int64(f.fs.chunkSize)
	startChunk := offset / chunkSize
	endChunk := (offset + length - 1) / chunkSize

	rows, err := f.fs.db.QueryContext(ctx, queryChunkRange, f.ino, startChunk, endChunk)
	if err != nil {
		return 0, err
	}
	defer rows.Close()

	// Collect chunks
	chunks := make(map[int64][]byte)
	for rows.Next() {
		var chunkIndex int64
		var data []byte
		if err := rows.Scan(&chunkIndex, &data); err != nil {
			return 0, err
		}
		chunks[chunkIndex] = data
	}
	if err := rows.Err(); err != nil {
		return 0, err
	}

	// Extract requested bytes
	bytesRead := 0
	currentOffset := offset

	for currentOffset < offset+length {
		chunkIndex := currentOffset / chunkSize
		offsetInChunk := currentOffset % chunkSize

		chunk, ok := chunks[chunkIndex]
		if !ok {
			// Sparse file - treat as zeros
			chunk = make([]byte, chunkSize)
		}

		// Calculate how many bytes to copy from this chunk
		bytesFromChunk := chunkSize - offsetInChunk
		remaining := offset + length - currentOffset
		if bytesFromChunk > remaining {
			bytesFromChunk = remaining
		}
		if offsetInChunk+bytesFromChunk > int64(len(chunk)) {
			bytesFromChunk = int64(len(chunk)) - offsetInChunk
		}

		copy(buf[bytesRead:], chunk[offsetInChunk:offsetInChunk+bytesFromChunk])
		bytesRead += int(bytesFromChunk)
		currentOffset += bytesFromChunk
	}

	// Update atime
	now := time.Now()
	f.fs.db.ExecContext(ctx, updateInodeAtime, now.Unix(), int64(now.Nanosecond()), f.ino)

	return bytesRead, nil
}

// Pwrite writes data at a specific offset (positioned write).
// Returns the number of bytes written and any error.
//
// Unlike Write, Pwrite does not modify the file's current offset.
func (f *File) Pwrite(ctx context.Context, data []byte, offset int64) (int, error) {
	if len(data) == 0 {
		return 0, nil
	}

	stats, err := f.fs.statInode(ctx, f.ino)
	if err != nil {
		return 0, err
	}

	chunkSize := int64(f.fs.chunkSize)
	endOffset := offset + int64(len(data))

	// Calculate affected chunks
	startChunk := offset / chunkSize
	endChunk := (endOffset - 1) / chunkSize

	// Read existing chunks that we'll partially overwrite
	existingChunks := make(map[int64][]byte)

	rows, err := f.fs.db.QueryContext(ctx, queryChunkRange, f.ino, startChunk, endChunk)
	if err != nil {
		return 0, err
	}
	for rows.Next() {
		var chunkIndex int64
		var chunkData []byte
		if err := rows.Scan(&chunkIndex, &chunkData); err != nil {
			rows.Close()
			return 0, err
		}
		existingChunks[chunkIndex] = chunkData
	}
	rows.Close()

	// Write data
	bytesWritten := 0
	currentOffset := offset
	dataOffset := 0

	for currentOffset < endOffset {
		chunkIndex := currentOffset / chunkSize
		offsetInChunk := currentOffset % chunkSize

		// Get or create chunk buffer
		chunk, exists := existingChunks[chunkIndex]
		if !exists {
			chunk = make([]byte, 0)
		}

		// Extend chunk if needed
		requiredLen := offsetInChunk + int64(len(data)-dataOffset)
		if requiredLen > chunkSize {
			requiredLen = chunkSize
		}
		if int64(len(chunk)) < requiredLen {
			newChunk := make([]byte, requiredLen)
			copy(newChunk, chunk)
			chunk = newChunk
		}

		// Calculate how many bytes to write to this chunk
		bytesToWrite := chunkSize - offsetInChunk
		remainingData := int64(len(data) - dataOffset)
		if bytesToWrite > remainingData {
			bytesToWrite = remainingData
		}

		// Write to chunk
		copy(chunk[offsetInChunk:], data[dataOffset:dataOffset+int(bytesToWrite)])

		// Store chunk
		if _, err := f.fs.db.ExecContext(ctx, insertChunk, f.ino, chunkIndex, chunk); err != nil {
			return bytesWritten, err
		}

		bytesWritten += int(bytesToWrite)
		currentOffset += bytesToWrite
		dataOffset += int(bytesToWrite)
	}

	// Update inode size if we extended the file
	newSize := stats.Size
	if endOffset > stats.Size {
		newSize = endOffset
	}

	now := time.Now()
	if _, err := f.fs.db.ExecContext(ctx, updateInodeSize, newSize, now.Unix(), int64(now.Nanosecond()), f.ino); err != nil {
		return bytesWritten, err
	}

	return bytesWritten, nil
}

// Truncate sets the file size.
func (f *File) Truncate(ctx context.Context, size int64) error {
	stats, err := f.fs.statInode(ctx, f.ino)
	if err != nil {
		return err
	}

	chunkSize := int64(f.fs.chunkSize)

	if size < stats.Size {
		// Shrinking: delete chunks beyond new size
		lastChunk := size / chunkSize
		if size%chunkSize == 0 && size > 0 {
			lastChunk--
		}

		// Delete chunks beyond the new last chunk
		if _, err := f.fs.db.ExecContext(ctx, deleteChunksFromIndex, f.ino, lastChunk+1); err != nil {
			return err
		}

		// Truncate the last chunk if needed
		if size > 0 {
			offsetInLastChunk := size % chunkSize
			if offsetInLastChunk > 0 {
				// Read the last chunk
				var chunk []byte
				err := f.fs.db.QueryRowContext(ctx, "SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?", f.ino, lastChunk).Scan(&chunk)
				if err == nil && int64(len(chunk)) > offsetInLastChunk {
					// Truncate and rewrite
					chunk = chunk[:offsetInLastChunk]
					if _, err := f.fs.db.ExecContext(ctx, insertChunk, f.ino, lastChunk, chunk); err != nil {
						return err
					}
				}
			}
		}
	} else if size > stats.Size {
		// Extending: just update size (sparse file behavior)
		// The missing chunks will be treated as zeros on read
	}

	now := time.Now()
	if _, err := f.fs.db.ExecContext(ctx, updateInodeSize, size, now.Unix(), int64(now.Nanosecond()), f.ino); err != nil {
		return err
	}

	return nil
}

// Fsync flushes data to storage.
// Note: This is effectively a no-op for SQLite in WAL mode as writes are already durable.
func (f *File) Fsync(ctx context.Context) error {
	// SQLite in WAL mode automatically handles durability
	// We could issue a PRAGMA wal_checkpoint here if strict durability is needed
	return nil
}

// Stat returns the file's current metadata.
func (f *File) Stat(ctx context.Context) (*Stats, error) {
	return f.fs.statInode(ctx, f.ino)
}

// Path returns the file's path.
func (f *File) Path() string {
	return f.path
}

// Size returns the current file size.
func (f *File) Size() (int64, error) {
	stats, err := f.fs.statInode(f.context(), f.ino)
	if err != nil {
		return 0, err
	}
	return stats.Size, nil
}

// Close closes the file handle.
func (f *File) Close() error {
	// Currently no resources to release
	// In the future, this could handle things like advisory locks
	return nil
}

// Reader returns an io.Reader that reads from the current offset.
// This is useful when you need to pass the file to functions expecting io.Reader.
func (f *File) Reader() io.Reader {
	return f
}

// Writer returns an io.Writer that writes at the current offset.
// This is useful when you need to pass the file to functions expecting io.Writer.
func (f *File) Writer() io.Writer {
	return f
}

// ReadSeeker returns an io.ReadSeeker interface.
func (f *File) ReadSeeker() io.ReadSeeker {
	return f
}

// WriteSeeker returns an io.WriteSeeker interface.
func (f *File) WriteSeeker() io.WriteSeeker {
	return f
}
