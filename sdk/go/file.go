package agentfs

import (
	"context"
	"time"
)

// File represents an open file handle for read/write operations.
type File struct {
	fs    *Filesystem
	ino   int64
	path  string
	flags int
}

// Pread reads data at a specific offset (positioned read).
// Returns the number of bytes read and any error.
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
	now := time.Now().Unix()
	f.fs.db.ExecContext(ctx, updateInodeAtime, now, f.ino)

	return bytesRead, nil
}

// Pwrite writes data at a specific offset (positioned write).
// Returns the number of bytes written and any error.
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

	now := time.Now().Unix()
	if _, err := f.fs.db.ExecContext(ctx, updateInodeSize, newSize, now, f.ino); err != nil {
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

	now := time.Now().Unix()
	if _, err := f.fs.db.ExecContext(ctx, updateInodeSize, size, now, f.ino); err != nil {
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

// Close closes the file handle.
func (f *File) Close() error {
	// Currently no resources to release
	// In the future, this could handle things like advisory locks
	return nil
}
