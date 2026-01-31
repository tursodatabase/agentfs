package agentfs

import (
	"errors"
	"fmt"
)

// FSError represents a filesystem error with POSIX semantics
type FSError struct {
	Code    int    // POSIX error code
	Syscall string // Operation that failed (e.g., "stat", "open", "unlink")
	Path    string // Path that caused the error
	Message string // Human-readable error message
}

// Error implements the error interface
func (e *FSError) Error() string {
	if e.Message != "" {
		return fmt.Sprintf("%s %s: %s", e.Syscall, e.Path, e.Message)
	}
	return fmt.Sprintf("%s %s: %s", e.Syscall, e.Path, e.codeMessage())
}

// codeMessage returns a human-readable message for the error code
func (e *FSError) codeMessage() string {
	switch e.Code {
	case EPERM:
		return "operation not permitted"
	case ENOENT:
		return "no such file or directory"
	case EIO:
		return "input/output error"
	case EBADF:
		return "bad file descriptor"
	case EACCES:
		return "permission denied"
	case EEXIST:
		return "file exists"
	case ENOTDIR:
		return "not a directory"
	case EISDIR:
		return "is a directory"
	case EINVAL:
		return "invalid argument"
	case ENOSPC:
		return "no space left on device"
	case ENOSYS:
		return "function not implemented"
	case ENOTEMPTY:
		return "directory not empty"
	case ELOOP:
		return "too many levels of symbolic links"
	default:
		return fmt.Sprintf("error code %d", e.Code)
	}
}

// Is implements errors.Is for FSError
func (e *FSError) Is(target error) bool {
	var fsErr *FSError
	if errors.As(target, &fsErr) {
		return e.Code == fsErr.Code
	}
	return false
}

// NewFSError creates a new FSError
func NewFSError(code int, syscall, path, message string) *FSError {
	return &FSError{
		Code:    code,
		Syscall: syscall,
		Path:    path,
		Message: message,
	}
}

// Convenience constructors for common errors

// ErrNoent returns an ENOENT error (no such file or directory)
func ErrNoent(syscall, path string) *FSError {
	return &FSError{Code: ENOENT, Syscall: syscall, Path: path}
}

// ErrExist returns an EEXIST error (file exists)
func ErrExist(syscall, path string) *FSError {
	return &FSError{Code: EEXIST, Syscall: syscall, Path: path}
}

// ErrIsDir returns an EISDIR error (is a directory)
func ErrIsDir(syscall, path string) *FSError {
	return &FSError{Code: EISDIR, Syscall: syscall, Path: path}
}

// ErrNotDir returns an ENOTDIR error (not a directory)
func ErrNotDir(syscall, path string) *FSError {
	return &FSError{Code: ENOTDIR, Syscall: syscall, Path: path}
}

// ErrNotEmpty returns an ENOTEMPTY error (directory not empty)
func ErrNotEmpty(syscall, path string) *FSError {
	return &FSError{Code: ENOTEMPTY, Syscall: syscall, Path: path}
}

// ErrInval returns an EINVAL error (invalid argument)
func ErrInval(syscall, path, message string) *FSError {
	return &FSError{Code: EINVAL, Syscall: syscall, Path: path, Message: message}
}

// ErrPerm returns an EPERM error (operation not permitted)
func ErrPerm(syscall, path string) *FSError {
	return &FSError{Code: EPERM, Syscall: syscall, Path: path}
}

// ErrNosys returns an ENOSYS error (function not implemented)
func ErrNosys(syscall, path string) *FSError {
	return &FSError{Code: ENOSYS, Syscall: syscall, Path: path}
}

// ErrLoop returns an ELOOP error (too many symbolic links)
func ErrLoop(syscall, path string) *FSError {
	return &FSError{Code: ELOOP, Syscall: syscall, Path: path}
}

// IsNotExist returns true if the error indicates the file does not exist
func IsNotExist(err error) bool {
	var fsErr *FSError
	if errors.As(err, &fsErr) {
		return fsErr.Code == ENOENT
	}
	return false
}

// IsExist returns true if the error indicates the file already exists
func IsExist(err error) bool {
	var fsErr *FSError
	if errors.As(err, &fsErr) {
		return fsErr.Code == EEXIST
	}
	return false
}

// IsPermission returns true if the error indicates a permission problem
func IsPermission(err error) bool {
	var fsErr *FSError
	if errors.As(err, &fsErr) {
		return fsErr.Code == EPERM || fsErr.Code == EACCES
	}
	return false
}
