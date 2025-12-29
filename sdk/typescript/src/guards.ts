import type { DatabasePromise } from '@tursodatabase/database-common';
import { createFsError, type FsSyscall } from './errors.js';
import { S_IFDIR, S_IFLNK, S_IFMT } from './filesystem/interface.js';

async function getInodeMode(db: DatabasePromise, ino: number): Promise<number | null> {
  const stmt = db.prepare('SELECT mode FROM fs_inode WHERE ino = ?');
  const row = await stmt.get(ino) as { mode: number } | undefined;
  return row?.mode ?? null;
}

function isDirMode(mode: number): boolean {
  return (mode & S_IFMT) === S_IFDIR;
}

export async function getInodeModeOrThrow(
  db: DatabasePromise,
  ino: number,
  syscall: FsSyscall,
  path: string
): Promise<number> {
  const mode = await getInodeMode(db, ino);
  if (mode === null) {
    throw createFsError({
      code: 'ENOENT',
      syscall,
      path,
      message: 'no such file or directory',
    });
  }
  return mode;
}

export function assertNotRoot(path: string, syscall: FsSyscall): void {
  if (path === '/') {
    throw createFsError({
      code: 'EPERM',
      syscall,
      path,
      message: 'operation not permitted on root directory',
    });
  }
}

export function normalizeRmOptions(options?: { force?: boolean; recursive?: boolean }): {
  force: boolean;
  recursive: boolean;
} {
  return {
    force: options?.force === true,
    recursive: options?.recursive === true,
  };
}

export function throwENOENTUnlessForce(path: string, syscall: FsSyscall, force: boolean): void {
  if (force) return;
  throw createFsError({
    code: 'ENOENT',
    syscall,
    path,
    message: 'no such file or directory',
  });
}

export function assertNotSymlinkMode(mode: number, syscall: FsSyscall, path: string): void {
  if ((mode & S_IFMT) === S_IFLNK) {
    throw createFsError({
      code: 'ENOSYS',
      syscall,
      path,
      message: 'symbolic links not supported yet',
    });
  }
}

async function assertExistingNonDirNonSymlinkInode(
  db: DatabasePromise,
  ino: number,
  syscall: FsSyscall,
  fullPathForError: string
): Promise<void> {
  const mode = await getInodeMode(db, ino);
  if (mode === null) {
    throw createFsError({
      code: 'ENOENT',
      syscall,
      path: fullPathForError,
      message: 'no such file or directory',
    });
  }
  if (isDirMode(mode)) {
    throw createFsError({
      code: 'EISDIR',
      syscall,
      path: fullPathForError,
      message: 'illegal operation on a directory',
    });
  }
  assertNotSymlinkMode(mode, syscall, fullPathForError);
}

export async function assertInodeIsDirectory(
  db: DatabasePromise,
  ino: number,
  syscall: FsSyscall,
  fullPathForError: string
): Promise<void> {
  const mode = await getInodeMode(db, ino);
  if (mode === null) {
    throw createFsError({
      code: 'ENOENT',
      syscall,
      path: fullPathForError,
      message: 'no such file or directory',
    });
  }
  if (!isDirMode(mode)) {
    throw createFsError({
      code: 'ENOTDIR',
      syscall,
      path: fullPathForError,
      message: 'not a directory',
    });
  }
}

export async function assertWritableExistingInode(
  db: DatabasePromise,
  ino: number,
  syscall: FsSyscall,
  fullPathForError: string
): Promise<void> {
  await assertExistingNonDirNonSymlinkInode(db, ino, syscall, fullPathForError);
}

export async function assertReadableExistingInode(
  db: DatabasePromise,
  ino: number,
  syscall: FsSyscall,
  fullPathForError: string
): Promise<void> {
  await assertExistingNonDirNonSymlinkInode(db, ino, syscall, fullPathForError);
}

export async function assertReaddirTargetInode(
  db: DatabasePromise,
  ino: number,
  fullPathForError: string
): Promise<void> {
  const syscall = 'scandir';
  const mode = await getInodeMode(db, ino);
  if (mode === null) {
    throw createFsError({
      code: 'ENOENT',
      syscall,
      path: fullPathForError,
      message: 'no such file or directory',
    });
  }
  assertNotSymlinkMode(mode, syscall, fullPathForError);
  if (!isDirMode(mode)) {
    throw createFsError({
      code: 'ENOTDIR',
      syscall,
      path: fullPathForError,
      message: 'not a directory',
    });
  }
}

export async function assertUnlinkTargetInode(
  db: DatabasePromise,
  ino: number,
  fullPathForError: string
): Promise<void> {
  const syscall = 'unlink';
  const mode = await getInodeMode(db, ino);
  if (mode === null) {
    throw createFsError({
      code: 'ENOENT',
      syscall,
      path: fullPathForError,
      message: 'no such file or directory',
    });
  }
  if (isDirMode(mode)) {
    throw createFsError({
      code: 'EISDIR',
      syscall,
      path: fullPathForError,
      message: 'illegal operation on a directory',
    });
  }
  assertNotSymlinkMode(mode, syscall, fullPathForError);
}