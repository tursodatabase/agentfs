export {
  S_IFMT,
  S_IFREG,
  S_IFDIR,
  S_IFLNK,
  DEFAULT_FILE_MODE,
  DEFAULT_DIR_MODE,
  createStats,
} from './interface.js';

export type {
  Stats,
  StatsData,
  DirEntry,
  FilesystemStats,
  FileHandle,
  FileSystem,
} from './interface.js';

export { AgentFS } from './agentfs.js';
