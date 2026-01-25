package agentfs

import (
	"context"
	"database/sql"

	_ "turso.tech/database/tursogo"
)

func getInodeMode(db *sql.DB, ino int) (int, error) {
	ctx := context.Background()
	stmt, err := db.PrepareContext(ctx, "SELECT mode FROM fs_inode WHERE ino = ?")
	if err != nil {
		return 0, err
	}
	var result struct {
		mode int
	}
	row := stmt.QueryRowContext(ctx, ino)
	err = row.Scan(&result)
	if err != nil {
		return 0, nil
	}
	err = stmt.Close()
	if err != nil {
		return 0, nil
	}
	return result.mode, nil
}

func isDirMode(mode int) bool {
	return (mode & S_IFMT) == S_IFDIR
}

func assertInodeIsDirectory(
	db *sql.DB,
	ino int,
	syscall FsSyscall,
	fullPathForError string,
) error {
	mode, err := getInodeMode(db, ino)
	if err != nil {
		message := "no such file or directory"
		return &ErrnoException{
			Code:    ErrNoEnt,
			Syscall: &syscall,
			Path:    &fullPathForError,
			Message: &message,
		}
	}
	if !isDirMode(mode) {
		message := "not a directory"
		return &ErrnoException{
			Code:    ErrNotDir,
			Syscall: &syscall,
			Path:    &fullPathForError,
			Message: &message,
		}
	}
	return nil
}

func assertExistingNonDirNonSymlinkInode(
	db *sql.DB,
	ino int,
	syscall FsSyscall,
	fullPathForError string,
) error {
	mode, err := getInodeMode(db, ino)
	if err != nil {
		message := "no such file or directory"
		return &ErrnoException{
			Code:    ErrNoEnt,
			Syscall: &syscall,
			Path:    &fullPathForError,
			Message: &message,
		}
	}
	if isDirMode(mode) {
		message := "illegal operation on a directory"
		return &ErrnoException{
			Code:    ErrIsDir,
			Syscall: &syscall,
			Path:    &fullPathForError,
			Message: &message,
		}
	}
	assertNotSymlinkMode(mode, syscall, fullPathForError)
	return nil
}

func assertNotSymlinkMode(mode int, syscall FsSyscall, path string) error {
	if (mode & S_IFMT) == S_IFLNK {
		message := "symbolic links not supported yet"
		return &ErrnoException{
			Code:    ErrNoSys,
			Syscall: &syscall,
			Path:    &path,
			Message: &message,
		}
	}
	return nil
}

func assertWritableExistingInode(
	db *sql.DB,
	ino int,
	syscall FsSyscall,
	fullPathForError string,
) error {
	return assertExistingNonDirNonSymlinkInode(db, ino, syscall, fullPathForError)
}
