package agentfs

import (
	"context"
	"database/sql"
	"errors"
	"math"
	"slices"
	"strconv"
	"strings"
	"time"

	_ "turso.tech/database/tursogo"
)

const DefaultChunkSize int = 4096

var (
	_ FileHandle = (*AgentFSFile)(nil)
	_ FileSystem = (*AgentFS)(nil)
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

type AgentFS struct {
	db        *sql.DB
	rootIno   int
	chunkSize int
}

func NewAgentFS(db *sql.DB) *AgentFS {
	return &AgentFS{
		db:        db,
		rootIno:   1,
		chunkSize: DefaultChunkSize,
	}
}

func AgentFSFromDabatase(db *sql.DB) *AgentFS {
	agentfs := &AgentFS{
		db:        db,
		rootIno:   1,
		chunkSize: DefaultChunkSize,
	}
	agentfs.initialize()
	return agentfs
}

func (this *AgentFS) GetChunkSize() int {
	return this.chunkSize
}

func (this *AgentFS) initialize() (err error) {
	ctx := context.Background()
	_, err = this.db.ExecContext(ctx, `
      CREATE TABLE IF NOT EXISTS fs_config (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      )
    `)
	if err != nil {
		return
	}

	_, err = this.db.ExecContext(ctx, `
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
      )
    `)
	if err != nil {
		return
	}

	_, err = this.db.ExecContext(ctx, `
      CREATE TABLE IF NOT EXISTS fs_dentry (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        parent_ino INTEGER NOT NULL,
        ino INTEGER NOT NULL,
        UNIQUE(parent_ino, name)
      )
    `)
	if err != nil {
		return
	}
	_, err = this.db.ExecContext(ctx, `
      CREATE INDEX IF NOT EXISTS idx_fs_dentry_parent
      ON fs_dentry(parent_ino, name)
    `)
	if err != nil {
		return
	}

	_, err = this.db.ExecContext(ctx, `
      CREATE TABLE IF NOT EXISTS fs_data (
        ino INTEGER NOT NULL,
        chunk_index INTEGER NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (ino, chunk_index)
      )
    `)
	if err != nil {
		return
	}

	_, err = this.db.ExecContext(ctx, `
      CREATE TABLE IF NOT EXISTS fs_symlink (
        ino INTEGER PRIMARY KEY,
        target TEXT NOT NULL
      )
    `)
	if err != nil {
		return
	}

	this.chunkSize, err = this.ensureRoot()
	return
}

func (this *AgentFS) ensureRoot() (chunkSize int, err error) {
	ctx := context.Background()
	configStmt, err := this.db.PrepareContext(ctx,
		"SELECT value FROM fs_config WHERE key = 'chunk_size'",
	)
	if err != nil {
		return
	}
	row := configStmt.QueryRowContext(ctx)
	var config struct {
		value string
	}
	err = row.Scan(&config)
	if err != nil {
		if !errors.Is(err, sql.ErrNoRows) {
			return
		} else {
			insertConfigStmt, err := this.db.PrepareContext(ctx, `
			     INSERT INTO fs_config (key, value) VALUES ('chunk_size', ?)
			   `)
			if err != nil {
				return 0, err
			}
			_, err = insertConfigStmt.ExecContext(ctx, strconv.Itoa(DefaultChunkSize))
			if err != nil {
				return 0, err
			}
			chunkSize = DefaultChunkSize
			err = insertConfigStmt.Close()
			if err != nil {
				return 0, err
			}
		}
	}
	chunkSize, err = strconv.Atoi(config.value)
	if err != nil {
		return
	}
	stmt, err := this.db.PrepareContext(ctx, "SELECT ino FROM fs_inode WHERE ino = ?")
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	rootRow := stmt.QueryRowContext(ctx, this.rootIno)
	var ino int
	err = rootRow.Scan(&ino)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			now := int(math.Floor(float64(time.Now().UnixMilli() / 1000)))
			insertStmt, err := this.db.PrepareContext(ctx, `
			     INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime)
			     VALUES (?, ?, 1, 0, 0, 0, ?, ?, ?)
			   `)
			if err != nil {
				return 0, err
			}
			_, err = insertStmt.ExecContext(ctx, this.rootIno, DEFAULT_DIR_MODE, now, now, now)
			if err != nil {
				return 0, err
			}
			err = insertStmt.Close()
			if err != nil {
				return 0, err
			}
		} else {
			return
		}
	}
	return
}

func (this *AgentFS) normalizePath(path string) string {
	normalized := strings.TrimRight(path, "/")
	if normalized == "" {
		normalized = "/"
	}
	if !strings.HasPrefix(normalized, "/") {
		normalized = "/" + normalized
	}
	return normalized
}

func (this *AgentFS) splitPath(path string) []string {
	normalized := this.normalizePath(path)
	if normalized == "/" {
		return []string{}
	}
	parts := strings.Split(normalized, "/")
	if len(parts) > 0 && parts[0] == "" {
		return parts[1:]
	}
	return parts
}

func (this *AgentFS) resolvePathOrThrow(
	path string,
	syscall FsSyscall,
) (*struct {
	normalizedPath string
	ino            int
}, error) {
	normalizedPath := this.normalizePath(path)
	ino, err := this.resolvePath(normalizedPath)
	if err != nil {
		message := "no such file or directory"
		return nil, &ErrnoException{
			Code:    ErrNoEnt,
			Syscall: &syscall,
			Path:    &normalizedPath,
			Message: &message,
		}
	}
	return &struct {
		normalizedPath string
		ino            int
	}{
		normalizedPath: normalizedPath,
		ino:            ino,
	}, nil
}

func (this *AgentFS) resolvePath(path string) (int, error) {
	ctx := context.Background()
	normalized := this.normalizePath(path)

	if normalized == "/" {
		return this.rootIno, nil
	}

	parts := this.splitPath(normalized)
	currentIno := this.rootIno

	for _, name := range parts {
		stmt, err := this.db.PrepareContext(ctx, `
      SELECT ino FROM fs_dentry
      WHERE parent_ino = ? AND name = ?
    `)
		if err != nil {
			return 0, err
		}
		var result struct {
			ino int
		}
		row := stmt.QueryRowContext(ctx, currentIno, name)
		err = row.Scan(&result)
		if err != nil {
			return 0, err
		}
		currentIno = result.ino
		err = stmt.Close()
		if err != nil {
			return 0, err
		}
	}

	return currentIno, nil
}

func (this *AgentFS) resolveParent(path string) (*struct {
	parentIno int
	name      string
}, error) {
	normalized := this.normalizePath(path)
	if normalized == "/" {
		return nil, nil
	}
	parts := this.splitPath(normalized)
	name := parts[len(parts)-1]
	var parentPath string
	if len(parts) == 1 {
		parentPath = "/"
	} else {
		parentPath = "/" + strings.Join(parts[0:len(parts)-1], "/")
	}
	parentIno, err := this.resolvePath(parentPath)
	if err != nil {
		return nil, err
	}
	return &struct {
		parentIno int
		name      string
	}{parentIno: parentIno, name: name}, nil
}

func (this *AgentFS) createInode(
	mode int,
	uid int,
	gid int,
) (int, error) {
	ctx := context.Background()
	now := int(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	insertStmt, err := this.db.PrepareContext(ctx, `
  		INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime)
    	VALUES (?, ?, ?, 0, ?, ?, ?)
     	RETURNING ino
	   `)
	if err != nil {
		return 0, err
	}
	row := insertStmt.QueryRowContext(ctx, mode, uid, gid, now, now, now)
	var ino int
	err = row.Scan(&ino)
	if err != nil {
		return 0, err
	}
	err = insertStmt.Close()
	if err != nil {
		return 0, err
	}
	return ino, nil
}

func (this *AgentFS) createDentry(
	parentIno int,
	name string,
	ino int,
) (err error) {
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
     INSERT INTO fs_dentry (name, parent_ino, ino)
     VALUES (?, ?, ?)
   `)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	_, err = stmt.ExecContext(ctx, parentIno, ino)
	if err != nil {
		return
	}
	updateStmt, err := this.db.PrepareContext(ctx, "UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?")
	if err != nil {
		return
	}
	defer func() { err = updateStmt.Close() }()
	_, err = updateStmt.ExecContext(ctx, ino)
	return
}

func (this *AgentFS) ensureParentDirs(path string) error {
	parts := this.splitPath(path)
	parts = parts[:len(parts)-1]
	ctx := context.Background()
	currentIno := this.rootIno
	for _, name := range parts {
		stmt, err := this.db.PrepareContext(ctx, `
       	SELECT ino FROM fs_dentry
        WHERE parent_ino = ? AND name = ?
       `)
		if err != nil {
			return err
		}
		var result struct {
			ino int
		}
		row := stmt.QueryRowContext(ctx, currentIno, name)
		err = row.Scan(&result)
		if err != nil {
			if errors.Is(err, sql.ErrNoRows) {
				dirIno, err := this.createInode(DEFAULT_DIR_MODE, 0, 0)
				if err != nil {
					return err
				}
				this.createDentry(currentIno, name, dirIno)
			} else {
				return err
			}
		}
		err = assertInodeIsDirectory(this.db, result.ino, Open, this.normalizePath(path))
		if err != nil {
			return err
		}
		currentIno = result.ino
	}
	return nil
}

func (this *AgentFS) getLinkCount(ino int) (int, error) {
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, "SELECT nlink FROM fs_inode WHERE ino = ?")
	if err != nil {
		return 0, err
	}
	var result struct {
		nlink int
	}
	row := stmt.QueryRowContext(ctx, ino)
	err = row.Scan(&result)
	if err != nil {
		return 0, err
	}
	err = stmt.Close()
	if err != nil {
		return 0, err
	}
	return result.nlink, nil
}

func (this *AgentFS) getInodeMode(ino int) (int, error) {
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, "SELECT mode FROM fs_inode WHERE ino = ?")
	var result struct {
		mode int
	}
	row := stmt.QueryRowContext(ctx, ino)
	err = row.Scan(&result)
	if err != nil {
		return 0, err
	}
	err = stmt.Close()
	if err != nil {
		return 0, err
	}
	return result.mode, nil
}

func (this *AgentFS) WriteFile(
	path string,
	content []byte,
) (err error) {
	err = this.ensureParentDirs(path)
	if err != nil {
		return
	}
	normalizedPath := this.normalizePath(path)
	ino, err := this.resolvePath(path)
	var parent struct {
		parentIno int
		name      string
	}
	if err != nil {
		assertWritableExistingInode(this.db, ino, Open, normalizedPath)
		err := this.updateFileContent(ino, content)
		if err != nil {
			return err
		}
	} else {
		parentPtr, err := this.resolveParent(path)
		if err != nil {
			scall := Open
			message := "no such file or directory"
			return &ErrnoException{
				Code:    ErrNoEnt,
				Syscall: &scall,
				Path:    &normalizedPath,
				Message: &message,
			}
		}
		parent = *parentPtr
	}
	err = assertInodeIsDirectory(this.db, parent.parentIno, Open, normalizedPath)
	if err != nil {
		return
	}
	fileIno, err := this.createInode(DEFAULT_FILE_MODE, 0, 0)
	if err != nil {
		return
	}
	err = this.createDentry(parent.parentIno, parent.name, fileIno)
	if err != nil {
		return
	}
	return this.updateFileContent(fileIno, content)

}

func (this *AgentFS) updateFileContent(
	ino int,
	content []byte,
) (err error) {
	now := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	ctx := context.Background()
	deleteStmt, err := this.db.PrepareContext(ctx, "DELETE FROM fs_data WHERE ino = ?")
	if err != nil {
		return
	}
	defer func() { err = deleteStmt.Close() }()
	_, err = deleteStmt.ExecContext(ctx, ino)
	if len(content) > 0 {
		stmt, err := this.db.PrepareContext(ctx, `
			INSERT INTO fs_data (ino, chunk_index, data)
       		VALUES (?, ?, ?)
         `)
		if err != nil {
			return err
		}
		chunkIndex := 0
		for offset := 0; offset < len(content); offset += this.chunkSize {
			maxIdx := min(offset+this.chunkSize, len(content))
			chunk := content[offset:maxIdx]
			_, err := stmt.ExecContext(ctx, ino, chunkIndex, chunk)
			if err != nil {
				return err
			}
			chunkIndex++
		}
		err = stmt.Close()
		if err != nil {
			return err
		}
	}
	updateStmt, err := this.db.PrepareContext(ctx, `
      UPDATE fs_inode
      SET size = ?, mtime = ?
      WHERE ino = ?
     `)
	if err != nil {
		return
	}
	defer func() { err = updateStmt.Close() }()
	_, err = updateStmt.ExecContext(ctx, len(content), now, ino)
	return
}
