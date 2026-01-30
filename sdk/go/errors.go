package agentfs

import "fmt"

var (
	_ error = (*ErrnoException)(nil)
)

// POSIX-style error codes for filesystem operations.
type FsErrorCode string

const (
	ErrNoEnt    FsErrorCode = "ENOENT"    // No such file or directory
	ErrExist    FsErrorCode = "EEXIST"    // File already exists
	ErrIsDir    FsErrorCode = "EISDIR"    // Is a directory
	ErrNotDir   FsErrorCode = "ENOTDIR"   // Not a directory
	ErrNotEmpty FsErrorCode = "ENOTEMPTY" // Directory not empty
	ErrPerm     FsErrorCode = "EPERM"     // Operation not permitted
	ErrInvalid  FsErrorCode = "EINVAL"    // Invalid argument
	ErrNoSys    FsErrorCode = "ENOSYS"    // Function not implemented
)

// Filesystem syscall names for error reporting.
// rm, scandir and copyFile are not actual syscall but used for convenience
type FsSyscall string

const (
	Open     FsSyscall = "open"
	Stat     FsSyscall = "stat"
	Mkdir    FsSyscall = "mkdir"
	Rm       FsSyscall = "rm"
	Rmdir    FsSyscall = "rmdir"
	Unlink   FsSyscall = "unlink"
	Scandir  FsSyscall = "scandir"
	Rename   FsSyscall = "rename"
	CopyFile FsSyscall = "copyfile"
	Access   FsSyscall = "access"
)

type ErrnoException struct {
	Code    FsErrorCode
	Syscall *FsSyscall
	Path    *string
	Message *string
}

// ErrnoException implements the error interface
func (e *ErrnoException) Error() string {
	var syscall string
	switch e.Syscall {
	case nil:
		syscall = "no syscall"
	default:
		syscall = string(*e.Syscall)
	}
	var path string
	switch e.Path {
	case nil:
		path = "no path"
	default:
		path = *e.Path
	}
	var msg string
	switch e.Message {
	case nil:
		msg = "no message"
	default:
		msg = *e.Message
	}
	return fmt.Sprintf("%s: %s, %s %s", e.Code, msg, syscall, path)
}
