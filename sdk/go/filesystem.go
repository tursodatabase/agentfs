package agentfs

import (
	"context"
	"database/sql"
	"errors"
	"math"
	"slices"
	"time"

	_ "turso.tech/database/tursogo"
)

var (
	_ FileHandle = (*AgentFSFile)(nil)
)

type AgentFSFile struct {
	db        *sql.DB
	ino       int
	chunkSize int
}

func NewAgentFSFile(db *sql.DB, ino, chunkSize int) *AgentFSFile {
	return &AgentFSFile{
		db:        db,
		ino:       ino,
		chunkSize: chunkSize,
	}
}

func (f *AgentFSFile) Pread(offset, size int) (data []byte, err error) {
	ctx := context.Background()
	startChunk := int(math.Floor(float64(offset / f.chunkSize)))
	endChunk := int(math.Floor(float64((offset + size - 1) / f.chunkSize)))
	stmt, err := f.db.PrepareContext(ctx, `
      SELECT chunk_index, data FROM fs_data
      WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ?
      ORDER BY chunk_index ASC
    `)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	rows, err := stmt.QueryContext(ctx, f.ino, startChunk, endChunk)
	if err != nil {
		return
	}
	buffers := [][]byte{}
	bytesCollected := 0
	startOffsetInChunk := offset % f.chunkSize
	for rows.Next() {
		var rowStruct struct {
			chunk_index int
			data        []byte
		}
		err = rows.Scan(
			&rowStruct.chunk_index,
			&rowStruct.data,
		)
		var skip int
		if len(buffers) == 0 {
			skip = startOffsetInChunk
		} else {
			skip = 0
		}
		if skip >= len(rowStruct.data) {
			continue
		}
		remaining := size - bytesCollected
		take := min(len(rowStruct.data)-skip, remaining)
		buffers = append(buffers, rowStruct.data[skip:skip+take])
		bytesCollected += take
	}
	if len(buffers) == 0 {
		return
	}
	data = slices.Concat(buffers...)
	return
}

func (f *AgentFSFile) Pwrite(offset int, data []byte) (err error) {
	if len(data) == 0 {
		return
	}
	ctx := context.Background()
	sizeStmt, err := f.db.PrepareContext(ctx, `SELECT size FROM fs_inode WHERE ino = ?`)
	if err != nil {
		return
	}
	defer func() { err = sizeStmt.Close() }()
	row := sizeStmt.QueryRowContext(ctx, f.ino)
	var sizeRow struct{ size int }
	var currentSize int
	err = row.Scan(&sizeRow.size)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			currentSize = 0
		} else {
			return
		}
	} else {
		currentSize = sizeRow.size
	}
	if currentSize < offset {
		zeros := make([]byte, offset-currentSize)
		err = f.writeDataAtOffset(currentSize, zeros)
		if err != nil {
			return
		}
	}
	err = f.writeDataAtOffset(offset, data)
	if err != nil {
		return
	}
	newSize := max(currentSize, offset+len(data))
	now := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	updateStmt, err := f.db.PrepareContext(ctx, `UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?`)
	if err != nil {
		return
	}
	defer func() { err = updateStmt.Close() }()
	_, err = updateStmt.ExecContext(ctx, newSize, now, f.ino)
	return
}

func (f *AgentFSFile) writeDataAtOffset(offset int, data []byte) (err error) {
	ctx := context.Background()
	startChunk := int(math.Floor(float64(offset / f.chunkSize)))
	endChunk := int(math.Floor(float64(offset+len(data)-1) / float64(f.chunkSize)))
	for chunkIdx := startChunk; chunkIdx <= endChunk; chunkIdx++ {
		chunkStart := chunkIdx * f.chunkSize
		chunkEnd := chunkStart + f.chunkSize
		dataStart := max(0, chunkStart-offset)
		dataEnd := min(len(data), chunkEnd-offset)
		writeOffset := max(0, offset-chunkStart)
		selectStmt, err := f.db.PrepareContext(ctx, `SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?`)
		if err != nil {
			return err
		}
		var existingRow struct{ data []byte }
		var existingData []byte
		row := selectStmt.QueryRowContext(ctx, f.ino, chunkIdx)
		err = row.Scan(&existingRow.data)
		if err != nil {
			if errors.Is(err, sql.ErrNoRows) {
				existingData = nil
			} else {
				return err
			}
		} else {
			existingData = existingRow.data
		}
		var chunkData []byte
		if existingData != nil {
			chunkData = existingData
			if (writeOffset + (dataEnd - dataStart)) > len(chunkData) {
				newChunk := make([]byte, writeOffset+(dataEnd-dataStart))
				copy(newChunk, chunkData)
				chunkData = newChunk
			}
		} else {
			chunkData = make([]byte, writeOffset+(dataEnd-dataStart))
		}
		copy(chunkData[writeOffset:], data[dataStart:dataEnd])
		upsertStmt, err := f.db.PrepareContext(ctx, `
        	INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)
        	ON CONFLICT(ino, chunk_index) DO UPDATE SET data = excluded.data
      	`)
		if err != nil {
			return err
		}
		_, err = upsertStmt.ExecContext(ctx, f.ino, chunkIdx, chunkData)
		err = selectStmt.Close()
		if err != nil {
			return err
		}
		err = upsertStmt.Close()
		if err != nil {
			return err
		}

	}
	return nil
}

func (f *AgentFSFile) Truncate(newSize int) (err error) {
	ctx := context.Background()
	sizeStmt, err := f.db.PrepareContext(ctx, `SELECT size FROM fs_inode WHERE ino = ?`)
	if err != nil {
		return
	}
	defer func() { err = sizeStmt.Close() }()
	row := sizeStmt.QueryRowContext(ctx, f.ino)
	var sizeRow struct{ size int }
	var currentSize int
	err = row.Scan(&sizeRow.size)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			currentSize = 0
		} else {
			return
		}
	} else {
		currentSize = sizeRow.size
	}
	_, err = f.db.ExecContext(ctx, "BEGIN")
	if err != nil {
		return
	}
	if newSize == 0 {
		deleteStmt, err := f.db.PrepareContext(ctx, `DELETE FROM fs_data WHERE ino = ?`)
		if err != nil {
			return err
		}
		_, err = deleteStmt.ExecContext(ctx, f.ino)
		if err != nil {
			return err
		}
		err = deleteStmt.Close()
		if err != nil {
			return err
		}
	} else if newSize < currentSize {
		lastChunkId := int(math.Floor((float64((newSize - 1) / f.chunkSize))))
		deleteStmt, err := f.db.PrepareContext(ctx, `DELETE FROM fs_data WHERE ino = ? AND chunk_index > ?`)
		if err != nil {
			return err
		}
		_, err = deleteStmt.ExecContext(ctx, f.ino, lastChunkId)
		if err != nil {
			return err
		}
		err = deleteStmt.Close()
		if err != nil {
			return err
		}
		offsetInChunk := newSize % f.chunkSize
		if offsetInChunk > 0 {
			selectStmt, err := f.db.PrepareContext(ctx, `SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?`)
			if err != nil {
				return err
			}
			row := selectStmt.QueryRowContext(ctx, f.ino, lastChunkId)
			var rowStruct struct{ data []byte }
			var dataToUse []byte
			err = row.Scan(&rowStruct.data)
			if err != nil {
				if errors.Is(err, sql.ErrNoRows) {
					dataToUse = nil
				} else {
					return err
				}
			} else {
				dataToUse = rowStruct.data
			}
			if len(dataToUse) > 0 {
				trunkatedChunk := dataToUse[:offsetInChunk]
				updateStmt, err := f.db.PrepareContext(ctx, `UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?`)
				if err != nil {
					return err
				}
				_, err = updateStmt.ExecContext(ctx, trunkatedChunk, f.ino, lastChunkId)
				if err != nil {
					return err
				}
				err = updateStmt.Close()
				if err != nil {
					return err
				}
			}
		}
	}
	now := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	updateStmt, err := f.db.PrepareContext(ctx, `UPDATE fs_inode SET size = ?, mtime = ? WHERE ino = ?`)
	if err != nil {
		return
	}
	defer func() { err = updateStmt.Close() }()
	_, err = updateStmt.ExecContext(ctx, newSize, now, f.ino)
	if err != nil {
		return
	}
	_, err = f.db.ExecContext(ctx, "COMMIT")
	if err != nil {
		return
	}
	return
}

func (f *AgentFSFile) Fsync() (err error) {
	ctx := context.Background()
	_, err = f.db.ExecContext(ctx, `PRAGMA synchronous = FULL`)
	if err != nil {
		return
	}
	_, err = f.db.ExecContext(ctx, `PRAGMA wal_checkpoint(TRUNCATE)`)
	return
}

func (f *AgentFSFile) Fstat() (stats Stats, err error) {
	ctx := context.Background()
	stmt, err := f.db.PrepareContext(ctx, `
      SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime
      FROM fs_inode WHERE ino = ?
    `)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	var data DataStats
	row := stmt.QueryRowContext(ctx, f.ino)
	err = row.Scan(&data.Ino, &data.Mode, &data.Nlink, &data.Uid, &data.Gid, &data.Size, &data.Atime, &data.Mtime, &data.Ctime)
	if err != nil {
		return
	}
	stats = data
	return
}
