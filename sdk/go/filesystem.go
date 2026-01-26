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

func (this *AgentFS) ReadFile(path string) ([]byte, error) {
	s, err := this.resolvePathOrThrow(path, Open)
	if err != nil {
		return nil, err
	}
	normalizedPath := s.normalizedPath
	ino := s.ino
	err = assertReadableExistingInode(this.db, ino, Open, normalizedPath)
	if err != nil {
		return nil, err
	}
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
     SELECT data FROM fs_data
     WHERE ino = ?
     ORDER BY chunk_index ASC
   `)
	if err != nil {
		return nil, err
	}
	defer func() { err = stmt.Close() }()
	rows, err := stmt.QueryContext(ctx, ino)
	if err != nil {
		return nil, err
	}
	buffers := []struct {
		data []byte
	}{}
	for rows.Next() {
		var d struct{ data []byte }
		err = rows.Scan(&d)
		if err != nil {
			return nil, err
		}
		buffers = append(buffers, d)
	}
	buffer := []byte{}
	if len(buffers) != 0 {
		for _, d := range buffers {
			buffer = append(buffer, d.data...)
		}
	}
	now := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	updateStmt, err := this.db.PrepareContext(ctx, "UPDATE fs_inode SET atime = ? WHERE ino = ?")
	if err != nil {
		return nil, err
	}
	defer func() { err = updateStmt.Close() }()
	_, err = updateStmt.ExecContext(ctx, now, ino)
	if err != nil {
		return nil, err
	}
	return buffer, nil
}

func (this *AgentFS) Readdir(path string) ([]string, error) {
	s, err := this.resolvePathOrThrow(path, Open)
	if err != nil {
		return nil, err
	}
	normalizedPath := s.normalizedPath
	ino := s.ino
	err = assertReaddirTargetInode(this.db, ino, normalizedPath)
	if err != nil {
		return nil, err
	}
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
     SELECT name FROM fs_dentry
     WHERE parent_ino = ?
     ORDER BY name ASC
   `)
	if err != nil {
		return nil, err
	}
	defer func() { err = stmt.Close() }()
	rows, err := stmt.QueryContext(ctx, ino)
	if err != nil {
		return nil, err
	}
	names := []string{}
	for rows.Next() {
		var n struct{ name string }
		err := rows.Scan(&n)
		if err != nil {
			return nil, err
		}
		names = append(names, n.name)
	}
	return names, nil
}

func (this *AgentFS) ReaddirPlus(path string) ([]DirEntry, error) {
	s, err := this.resolvePathOrThrow(path, Open)
	if err != nil {
		return nil, err
	}
	normalizedPath := s.normalizedPath
	ino := s.ino
	err = assertReaddirTargetInode(this.db, ino, normalizedPath)
	if err != nil {
		return nil, err
	}
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
     SELECT d.name, i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime
     FROM fs_dentry d
     JOIN fs_inode i ON d.ino = i.ino
     WHERE d.parent_ino = ?
     ORDER BY d.name ASC
   `)
	if err != nil {
		return nil, err
	}
	defer func() { err = stmt.Close() }()
	rows, err := stmt.QueryContext(ctx, ino)
	if err != nil {
		return nil, err
	}
	entries := []struct {
		name  string
		ino   int
		mode  int
		nlink int
		uid   int
		gid   int
		size  int
		atime int
		mtime int
		ctime int
	}{}
	for rows.Next() {
		var entry struct {
			name  string
			ino   int
			mode  int
			nlink int
			uid   int
			gid   int
			size  int
			atime int
			mtime int
			ctime int
		}
		err := rows.Scan(&entry)
		if err != nil {
			return nil, err
		}
		entries = append(entries, entry)
	}
	dirEntries := make([]DirEntry, 0, len(entries))
	for _, entry := range entries {
		stats := NewDataStats(
			entry.ino,
			entry.mode,
			entry.nlink,
			entry.uid,
			entry.gid,
			entry.size,
			entry.atime,
			entry.mtime,
			entry.ctime,
		)
		name := entry.name
		dirEntry := DirEntry{name: name, stats: stats}
		dirEntries = append(dirEntries, dirEntry)
	}
	return dirEntries, nil
}

func (this *AgentFS) Stat(path string) (Stats, error) {
	s, err := this.resolvePathOrThrow(path, Stat)
	if err != nil {
		return nil, err
	}
	normalizedPath := s.normalizedPath
	ino := s.ino
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
     SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime
     FROM fs_inode
     WHERE ino = ?
   `)
	if err != nil {
		return nil, err
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, ino)
	var stats struct {
		ino   int
		mode  int
		nlink int
		uid   int
		gid   int
		size  int
		atime int
		mtime int
		ctime int
	}
	err = row.Scan(&stats)
	if err != nil {
		scall := Stat
		message := "no such file or directory"
		return nil, &ErrnoException{
			Code:    ErrNoEnt,
			Path:    &normalizedPath,
			Message: &message,
			Syscall: &scall,
		}
	}
	return NewDataStats(
		stats.ino,
		stats.mode,
		stats.nlink,
		stats.uid,
		stats.gid,
		stats.size,
		stats.atime,
		stats.mtime, stats.ctime,
	), nil
}

func (this *AgentFS) Lstat(path string) (Stats, error) {
	return this.Stat(path)
}

func (this *AgentFS) Mkdir(path string) error {
	normalizedPath := this.normalizePath(path)
	_, err := this.resolvePath(normalizedPath)
	if err == nil {
		scall := Mkdir
		message := "file already exists"
		return &ErrnoException{
			Code:    ErrExist,
			Syscall: &scall,
			Path:    &normalizedPath,
			Message: &message,
		}
	}
	parent, err := this.resolveParent(normalizedPath)
	if err != nil {
		scall := Mkdir
		message := "no such file or directory"
		return &ErrnoException{
			Code:    ErrNoEnt,
			Path:    &normalizedPath,
			Message: &message,
			Syscall: &scall,
		}
	}
	err = assertInodeIsDirectory(this.db, parent.parentIno, Mkdir, normalizedPath)
	if err != nil {
		return err
	}
	dirIno, err := this.createInode(DEFAULT_DIR_MODE, 0, 0)
	if err != nil {
		return err
	}
	err = this.createDentry(parent.parentIno, parent.name, dirIno)
	if err != nil {
		scall := Mkdir
		message := "file already exists"
		return &ErrnoException{
			Code:    ErrExist,
			Syscall: &scall,
			Path:    &normalizedPath,
			Message: &message,
		}
	}
	return nil
}

func (this *AgentFS) Rmdir(path string) error {
	normalizedPath := this.normalizePath(path)
	assertNotRoot(path, Rmdir)
	s, err := this.resolvePathOrThrow(normalizedPath, Rmdir)
	if err != nil {
		return err
	}
	ino := s.ino
	mode, err := getInodeModeOrThrow(
		this.db,
		ino,
		Rmdir,
		normalizedPath,
	)
	if err != nil {
		return err
	}
	err = assertNotSymlinkMode(mode, Rmdir, normalizedPath)
	if err != nil {
		return err
	}
	if (mode & S_IFMT) != S_IFDIR {
		scall := Rmdir
		message := "not a directory"
		return &ErrnoException{
			Code:    ErrNotDir,
			Syscall: &scall,
			Message: &message,
			Path:    &normalizedPath,
		}
	}
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
     SELECT 1 as one FROM fs_dentry
     WHERE parent_ino = ?
     LIMIT 1
   `)
	if err != nil {
		return err
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, ino)
	var child struct{ one int }
	err = row.Scan(&child)
	if err != nil {
		if !errors.Is(err, sql.ErrNoRows) {
			return err
		}
	} else {
		scall := Rmdir
		message := "directory not empty"
		return &ErrnoException{
			Code:    ErrNotEmpty,
			Syscall: &scall,
			Path:    &normalizedPath,
			Message: &message,
		}
	}
	parent, err := this.resolveParent(normalizedPath)
	if err != nil {
		scall := Rmdir
		message := "operation not permitted"
		return &ErrnoException{
			Code:    ErrPerm,
			Syscall: &scall,
			Message: &message,
			Path:    &normalizedPath,
		}
	}
	return this.removeDentryAndMaybeInode(parent.parentIno, parent.name, ino)
}

func (this *AgentFS) removeDentryAndMaybeInode(
	parentIno int,
	name string,
	ino int,
) error {
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
    DELETE FROM fs_dentry
    WHERE parent_ino = ? AND name = ?
  `)
	if err != nil {
		return err
	}
	defer func() { err = stmt.Close() }()
	_, err = stmt.ExecContext(ctx, parentIno, name)
	if err != nil {
		return err
	}

	decrementStmt, err := this.db.PrepareContext(ctx,
		"UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?",
	)
	if err != nil {
		return err
	}
	defer func() { err = decrementStmt.Close() }()
	_, err = decrementStmt.ExecContext(ctx, ino)
	if err != nil {
		return err
	}

	linkCount, err := this.getLinkCount(ino)

	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return err
	}

	if linkCount == 0 {
		deleteInodeStmt, err := this.db.PrepareContext(ctx,
			"DELETE FROM fs_inode WHERE ino = ?",
		)
		if err != nil {
			return err
		}
		_, err = deleteInodeStmt.ExecContext(ctx, ino)
		if err != nil {
			return err
		}
		err = deleteInodeStmt.Close()
		if err != nil {
			return err
		}

		deleteDataStmt, err := this.db.PrepareContext(ctx,
			"DELETE FROM fs_data WHERE ino = ?",
		)
		if err != nil {
			return err
		}
		_, err = deleteDataStmt.ExecContext(ctx, ino)
		if err != nil {
			return err
		}
		err = deleteDataStmt.Close()
		if err != nil {
			return err
		}

		deleteSymlinkStmt, err := this.db.PrepareContext(ctx,
			"DELETE FROM fs_symlink WHERE ino = ?",
		)
		if err != nil {
			return err
		}
		_, err = deleteSymlinkStmt.ExecContext(ctx, ino)
		if err != nil {
			return err
		}
	}
	return nil
}

func (this *AgentFS) Unlink(path string) error {
	normalizedPath := this.normalizePath(path)
	err := assertNotRoot(normalizedPath, Unlink)
	if err != nil {
		return err
	}
	s, err := this.resolvePathOrThrow(normalizedPath, Unlink)
	if err != nil {
		return err
	}
	ino := s.ino
	err = assertUnlinkTargetInode(this.db, ino, normalizedPath)
	if err != nil {
		return err
	}
	parent, err := this.resolveParent(normalizedPath)
	if err != nil {
		return err
	}
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
     DELETE FROM fs_dentry
     WHERE parent_ino = ? AND name = ?
   `)
	if err != nil {
		return err
	}
	defer func() { err = stmt.Close() }()
	_, err = stmt.ExecContext(ctx, parent.parentIno, parent.name)
	decrementStmt, err := this.db.PrepareContext(ctx, "UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?")
	if err != nil {
		return err
	}
	defer func() { err = decrementStmt.Close() }()
	_, err = decrementStmt.ExecContext(ctx, ino)
	if err != nil {
		return err
	}
	linkCount, err := this.getLinkCount(ino)

	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return err
	}
	if linkCount == 0 {
		deleteInoStmt, err := this.db.PrepareContext(ctx, "DELETE FROM fs_inode WHERE ino = ?")
		if err != nil {
			return err
		}
		_, err = deleteInoStmt.ExecContext(ctx, ino)
		if err != nil {
			return err
		}
		err = deleteInoStmt.Close()
		if err != nil {
			return err
		}
		deleteDataStmt, err := this.db.PrepareContext(ctx, "DELETE FROM fs_data WHERE ino = ?")
		if err != nil {
			return err
		}
		_, err = deleteDataStmt.ExecContext(ctx, ino)
		if err != nil {
			return err
		}
		err = deleteDataStmt.Close()
		if err != nil {
			return err
		}
	}
	return nil
}

func (this *AgentFS) Rm(path string, rmOptions ...RmOptions) error {
	normalizedPath := this.normalizePath(path)
	rmOption := normalizeRmOptions(rmOptions...)
	err := assertNotRoot(normalizedPath, Rm)
	if err != nil {
		return err
	}
	ino, err := this.resolvePath(normalizedPath)
	if err != nil {
		throwENOENTUnlessForce(normalizedPath, Rm, rmOption.Force)
		return err
	}
	mode, err := getInodeModeOrThrow(this.db, ino, Rm, normalizedPath)
	if err != nil {
		return err
	}
	err = assertNotSymlinkMode(mode, Rm, normalizedPath)
	if err != nil {
		return err
	}
	parent, err := this.resolveParent(normalizedPath)
	if err != nil {
		scall := Rm
		message := "operation not permitted"
		return &ErrnoException{
			Code:    ErrPerm,
			Syscall: &scall,
			Path:    &normalizedPath,
			Message: &message,
		}
	}
	if (mode & S_IFMT) == S_IFDIR {
		if !rmOption.Recursive {
			scall := Rm
			message := "illegal operation on a directory"
			return &ErrnoException{
				Path:    &normalizedPath,
				Message: &message,
				Syscall: &scall,
				Code:    ErrIsDir,
			}
		}
		err := this.rmDirContentsRecursive(ino)
		if err != nil {
			return err
		}
		err = this.removeDentryAndMaybeInode(parent.parentIno, parent.name, ino)
		if err != nil {
			return err
		}
		return nil
	}
	return this.removeDentryAndMaybeInode(parent.parentIno, parent.name, ino)
}

func (this *AgentFS) rmDirContentsRecursive(dirIno int) error {
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, `
    SELECT name, ino FROM fs_dentry
    WHERE parent_ino = ?
    ORDER BY name ASC
  `)
	if err != nil {
		return err
	}
	defer func() { err = stmt.Close() }()
	rows, err := stmt.QueryContext(ctx, dirIno)
	if err != nil {
		return err
	}
	children := []struct {
		name string
		ino  int
	}{}
	for rows.Next() {
		var child struct {
			name string
			ino  int
		}
		err := rows.Scan(&child)
		if err != nil {
			return err
		}
		children = append(children, child)
	}

	for _, child := range children {
		mode, err := this.getInodeMode(child.ino)
		if err != nil {
			continue
		}

		if (mode & S_IFMT) == S_IFDIR {
			err := this.rmDirContentsRecursive(child.ino)
			if err != nil {
				return err
			}
			err = this.removeDentryAndMaybeInode(dirIno, child.name, child.ino)
			if err != nil {
				return err
			}
		} else {
			assertNotSymlinkMode(mode, "rm", "<symlink>")
			err = this.removeDentryAndMaybeInode(dirIno, child.name, child.ino)
			return err
		}
	}
	return nil
}

func (this *AgentFS) Rename(oldPath string, newPath string) (err error) {
	oldNormalized := this.normalizePath(oldPath)
	newNormalized := this.normalizePath(newPath)
	if oldNormalized == newNormalized {
		return nil
	}
	err = assertNotRoot(oldNormalized, Rename)
	if err != nil {
		return
	}
	err = assertNotRoot(newNormalized, Rename)
	if err != nil {
		return
	}
	oldParent, err := this.resolveParent(oldNormalized)
	if err != nil {
		scall := Rename
		message := "operation not permitted"
		return &ErrnoException{
			Code:    ErrPerm,
			Syscall: &scall,
			Message: &message,
			Path:    &oldNormalized,
		}
	}
	newParent, err := this.resolveParent(newNormalized)
	if err != nil {
		scall := Rename
		message := "operation not permitted"
		return &ErrnoException{
			Code:    ErrPerm,
			Syscall: &scall,
			Message: &message,
			Path:    &newNormalized,
		}
	}

	err = assertInodeIsDirectory(this.db, newParent.parentIno, Rename, newNormalized)
	if err != nil {
		return
	}
	ctx := context.Background()
	_, err = this.db.ExecContext(ctx, "BEGIN")
	if err != nil {
		return
	}
	oldResolved, err := this.resolvePathOrThrow(oldNormalized, Rename)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	oldIno := oldResolved.ino
	oldMode, err := getInodeModeOrThrow(this.db, oldIno, Rename, oldNormalized)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	oldIsDir := (oldMode & S_IFMT) == S_IFDIR
	if oldIsDir && strings.HasPrefix(newNormalized, oldNormalized+"/") {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		scall := Rename
		message := "invalid argument"
		return &ErrnoException{
			Code:    ErrInvalid,
			Syscall: &scall,
			Message: &message,
			Path:    &newNormalized,
		}
	}
	newIno, err := this.resolvePath(newNormalized)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	newMode, err := getInodeModeOrThrow(this.db, newIno, Rename, newNormalized)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	err = assertNotSymlinkMode(newMode, Rename, newNormalized)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	newIsDir := (newMode & S_IFMT) == S_IFDIR
	if newIsDir && !oldIsDir {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		scall := Rename
		message := "illegal operation on a directory"
		return &ErrnoException{
			Code:    ErrIsDir,
			Syscall: &scall,
			Message: &message,
			Path:    &newNormalized,
		}
	}
	if !newIsDir && oldIsDir {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		scall := Rename
		message := "not a directory"
		return &ErrnoException{
			Code:    ErrNotDir,
			Syscall: &scall,
			Message: &message,
			Path:    &newNormalized,
		}
	}
	if newIsDir {
		stmt, err := this.db.PrepareContext(ctx, `
           SELECT 1 as one FROM fs_dentry
           WHERE parent_ino = ?
           LIMIT 1
         `)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		row := stmt.QueryRowContext(ctx, newIno)
		var child struct {
			one int
		}
		err = row.Scan(&child)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		if err == nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			scall := Rename
			message := "directory not empty"
			return &ErrnoException{
				Code:    ErrNotEmpty,
				Syscall: &scall,
				Message: &message,
				Path:    &newNormalized,
			}
		}
		err = this.removeDentryAndMaybeInode(newParent.parentIno, newParent.name, newIno)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		err = stmt.Close()
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
	}
	stmt, err := this.db.PrepareContext(ctx, `
      UPDATE fs_dentry
      SET parent_ino = ?, name = ?
       WHERE parent_ino = ? AND name = ?
     `)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	defer func() { err = stmt.Close() }()
	_, err = stmt.ExecContext(ctx, newParent.parentIno, newParent.name, oldParent.parentIno, oldParent.name)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	now := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	updatInodeCtimeStmt, err := this.db.PrepareContext(ctx, `
       UPDATE fs_inode
       SET ctime = ?
       WHERE ino = ?
     `)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	defer func() { err = updatInodeCtimeStmt.Close() }()
	_, err = updatInodeCtimeStmt.ExecContext(ctx, now, oldIno)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	updateDirTimeStmt, err := this.db.PrepareContext(ctx, `
       UPDATE fs_inode
       SET mtime = ?, ctime = ?
       WHERE ino = ?
     `)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	defer func() { err = updateDirTimeStmt.Close() }()
	_, err = updateDirTimeStmt.ExecContext(ctx, now, now, oldParent.parentIno)
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	if newParent.parentIno != oldParent.parentIno {
		_, err := updateDirTimeStmt.ExecContext(ctx, now, now, newParent.parentIno)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
	}
	_, err = this.db.ExecContext(ctx, "COMMIT")
	return
}

func (this *AgentFS) CopyFile(src string, dest string) (err error) {
	srcNormalized := this.normalizePath(src)
	destNormalized := this.normalizePath(dest)
	if srcNormalized == destNormalized {
		scall := CopyFile
		message := "invalid argument"
		return &ErrnoException{
			Code:    ErrInvalid,
			Syscall: &scall,
			Message: &message,
			Path:    &destNormalized,
		}
	}
	srcResolved, err := this.resolvePathOrThrow(srcNormalized, CopyFile)
	if err != nil {
		return
	}
	srcIno := srcResolved.ino
	err = assertReadableExistingInode(this.db, srcIno, CopyFile, srcNormalized)
	if err != nil {
		return
	}
	ctx := context.Background()
	stmt, err := this.db.PrepareContext(ctx, "SELECT mode, uid, gid, size FROM fs_inode WHERE ino = ?")
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, srcIno)
	var srcRow struct {
		mode int
		uid  int
		gid  int
		size int
	}
	err = row.Scan(&srcRow)
	if err != nil && errors.Is(err, sql.ErrNoRows) {
		scall := CopyFile
		message := "no such file or directory"
		return &ErrnoException{
			Code:    ErrNoEnt,
			Syscall: &scall,
			Message: &message,
			Path:    &srcNormalized,
		}
	} else if err != nil {
		return
	}
	destParent, err := this.resolveParent(destNormalized)
	if err != nil {
		scall := CopyFile
		message := "no such file or directory"
		return &ErrnoException{
			Code:    ErrNoEnt,
			Message: &message,
			Path:    &destNormalized,
			Syscall: &scall,
		}
	}
	err = assertInodeIsDirectory(this.db, destParent.parentIno, CopyFile, destNormalized)
	_, err = this.db.ExecContext(ctx, "BEGIN")
	if err != nil {
		return
	}
	now := int64(math.Floor(float64(time.Now().UnixMilli() / 1000)))
	destIno, err := this.resolvePath(destNormalized)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	} else if err != nil && errors.Is(err, sql.ErrNoRows) {
		destMode, err := getInodeModeOrThrow(this.db, destIno, CopyFile, destNormalized)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		err = assertNotSymlinkMode(destMode, CopyFile, destNormalized)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		if (destMode & S_IFMT) == S_IFDIR {
			scall := CopyFile
			message := "illegal operation on a directory"
			return &ErrnoException{
				Code:    ErrIsDir,
				Message: &message,
				Syscall: &scall,
				Path:    &destNormalized,
			}
		}
		deleteStmt, err := this.db.PrepareContext(ctx, "DELETE FROM fs_data WHERE ino = ?")
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		defer func() { err = deleteStmt.Close() }()
		_, err = deleteStmt.ExecContext(ctx, destIno)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		copyStmt, err := this.db.PrepareContext(ctx, `
         INSERT INTO fs_data (ino, chunk_index, data)
         SELECT ?, chunk_index, data
         FROM fs_data
         WHERE ino = ?
         ORDER BY chunk_index ASC
       `)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		defer func() { err = copyStmt.Close() }()
		_, err = copyStmt.ExecContext(ctx, srcIno)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		updateStmt, err := this.db.PrepareContext(ctx, `
         UPDATE fs_inode
         SET mode = ?, uid = ?, gid = ?, size = ?, mtime = ?, ctime = ?
         WHERE ino = ?
       `)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		_, err = updateStmt.ExecContext(ctx, srcRow.mode, srcRow.uid, srcRow.gid, srcRow.size, now, now, destIno)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
	} else {
		destInoCreated, err := this.createInode(srcRow.mode, srcRow.uid, srcRow.gid)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		err = this.createDentry(destParent.parentIno, destParent.name, destInoCreated)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		copyStmt, err := this.db.PrepareContext(ctx, `
	         INSERT INTO fs_data (ino, chunk_index, data)
	         SELECT ?, chunk_index, data
	         FROM fs_data
	         WHERE ino = ?
	         ORDER BY chunk_index ASC
       `)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		defer func() { err = copyStmt.Close() }()
		_, err = copyStmt.ExecContext(ctx, destInoCreated, srcIno)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		updateStmt, err := this.db.PrepareContext(ctx, `
         UPDATE fs_inode
         SET size = ?, mtime = ?, ctime = ?
         WHERE ino = ?
       `)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
		_, err = updateStmt.ExecContext(ctx, srcRow.size, now, now, destInoCreated)
		if err != nil {
			_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
			if errRoll != nil {
				return errRoll
			}
			return err
		}
	}
	_, err = this.db.ExecContext(ctx, "COMMIT")
	if err != nil {
		_, errRoll := this.db.ExecContext(ctx, "ROLLBACK")
		if errRoll != nil {
			return errRoll
		}
		return
	}
	return
}

func (this *AgentFS) Access(path string) error {
	normalizedPath := this.normalizePath(path)
	_, err := this.resolvePath(normalizedPath)
	if err != nil {
		scall := Access
		message := "no such file or directory"
		return &ErrnoException{
			Code:    ErrNoEnt,
			Syscall: &scall,
			Path:    &normalizedPath,
			Message: &message,
		}
	}
	return nil
}

func (this *AgentFS) Statfs() (FilesystemStats, error) {
	ctx := context.Background()
	inodeStmt, err := this.db.PrepareContext(ctx, "SELECT COUNT(*) as count FROM fs_inode")
	if err != nil {
		return FilesystemStats{}, err
	}
	row := inodeStmt.QueryRowContext(ctx)
	var inodeRow struct {
		count int
	}
	defer func() { err = inodeStmt.Close() }()
	err = row.Scan(&inodeRow)
	if err != nil {
		return FilesystemStats{}, err
	}

	bytesStmt, err := this.db.PrepareContext(ctx, "SELECT COALESCE(SUM(LENGTH(data)), 0) as total FROM fs_data")
	if err != nil {
		return FilesystemStats{}, err
	}
	defer func() { err = bytesStmt.Close() }()
	bRow := bytesStmt.QueryRowContext(ctx)
	var bytesRow struct {
		total int
	}
	err = bRow.Scan(&bytesRow)
	if err != nil {
		return FilesystemStats{}, err
	}

	return FilesystemStats{
		Inodes:    inodeRow.count,
		BytesUsed: bytesRow.total,
	}, nil
}

func (this *AgentFS) Open(path string) (FileHandle, error) {
	resolvedPath, err := this.resolvePathOrThrow(path, Open)
	if err != nil {
		return nil, err
	}
	ino := resolvedPath.ino
	normalizedPath := resolvedPath.normalizedPath
	err = assertReadableExistingInode(this.db, ino, Open, normalizedPath)
	if err != nil {
		return nil, err
	}

	return &AgentFSFile{db: this.db, ino: ino, chunkSize: this.chunkSize}, nil
}
