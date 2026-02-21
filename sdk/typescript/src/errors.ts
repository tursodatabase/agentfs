/**
 * POSIX-style error codes for filesystem operations.
 */
export type FsErrorCode =
  | 'ENOENT'    // No such file or directory
  | 'EEXIST'    // File already exists
  | 'EISDIR'    // Is a directory (when file expected)
  | 'ENOTDIR'   // Not a directory (when directory expected)
  | 'ENOTEMPTY' // Directory not empty
  | 'EPERM'     // Operation not permitted
  | 'EINVAL'    // Invalid argument
  | 'ELOOP'     // Too many levels of symbolic links
  | 'ENOSYS';   // Function not implemented (use for symlinks)

/**
 * Filesystem syscall names for error reporting.
 * rm, scandir and copyFile are not actual syscall but used for convenience
 */
export type FsSyscall =
  | 'open'
  | 'stat'
  | 'lstat'
  | 'mkdir'
  | 'rmdir'
  | 'rm'
  | 'unlink'
  | 'rename'
  | 'scandir'
  | 'copyfile'
  | 'access';

export interface ErrnoException extends Error {
  code?: FsErrorCode;
  syscall?: FsSyscall;
  path?: string;
}

export function createFsError(params: {
  code: FsErrorCode;
  syscall: FsSyscall;
  path?: string;
  message?: string;
}): ErrnoException {
  const { code, syscall, path, message } = params;
  const base = message ?? code;
  const suffix =
    path !== undefined
      ? ` '${path}'`
      : '';
  const err = new Error(`${code}: ${base}, ${syscall}${suffix}`) as ErrnoException;
  err.code = code;
  err.syscall = syscall;
  if (path !== undefined) err.path = path;
  return err;
}
