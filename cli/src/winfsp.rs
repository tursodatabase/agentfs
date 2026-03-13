//! WinFsp filesystem adapter for AgentFS.
//!
//! This adapter implements the WinFsp FileSystemContext trait by wrapping
//! the agentfs_sdk::FileSystem trait. It allows mounting AgentFS on Windows.

use agentfs_sdk::error::Error as SdkError;
use agentfs_sdk::filesystem::TimeChange;
use agentfs_sdk::{BoxedFile, FileSystem, Stats};
use anyhow::Result;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    ffi::c_void,
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::runtime::Handle;
use tracing;
use winfsp::filesystem::{
    DirBuffer, DirInfo, FileInfo, FileSecurity, FileSystemContext,
    OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::{FspError, U16CStr};

// Windows file attribute constants
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x00000010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x00000020;
const FILE_ATTRIBUTE_READONLY: u32 = 0x00000001;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;

// Windows create options flags
const FILE_DIRECTORY_FILE: u32 = 0x00000001;

// NTSTATUS error codes (these are negative values when interpreted as i32)
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;
const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101u32 as i32;
const STATUS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
const STATUS_DISK_FULL: i32 = 0xC000_007Fu32 as i32;
const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033u32 as i32;

/// Convert an SDK error to a WinFsp error code.
fn error_to_ntstatus(e: &SdkError) -> i32 {
    match e {
        SdkError::Fs(fs_err) => {
            match fs_err {
                agentfs_sdk::filesystem::FsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
                agentfs_sdk::filesystem::FsError::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
                agentfs_sdk::filesystem::FsError::NotADirectory => STATUS_NOT_A_DIRECTORY,
                agentfs_sdk::filesystem::FsError::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
                agentfs_sdk::filesystem::FsError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
                agentfs_sdk::filesystem::FsError::InvalidPath => STATUS_OBJECT_NAME_INVALID,
                agentfs_sdk::filesystem::FsError::RootOperation => STATUS_ACCESS_DENIED,
                agentfs_sdk::filesystem::FsError::SymlinkLoop => STATUS_INVALID_PARAMETER,
                agentfs_sdk::filesystem::FsError::InvalidRename => STATUS_INVALID_PARAMETER,
                agentfs_sdk::filesystem::FsError::NameTooLong => STATUS_OBJECT_NAME_INVALID,
                agentfs_sdk::filesystem::FsError::NotASymlink => STATUS_INVALID_PARAMETER,
            }
        }
        SdkError::Io(io_err) => {
            match io_err.kind() {
                std::io::ErrorKind::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
                std::io::ErrorKind::PermissionDenied => STATUS_ACCESS_DENIED,
                std::io::ErrorKind::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
                std::io::ErrorKind::InvalidInput => STATUS_INVALID_PARAMETER,
                std::io::ErrorKind::StorageFull => STATUS_DISK_FULL,
                _ => STATUS_INVALID_PARAMETER,
            }
        }
        _ => STATUS_INVALID_PARAMETER,
    }
}

/// Convert an anyhow error to a WinFsp error code.
fn anyhow_to_ntstatus(e: &anyhow::Error) -> i32 {
    // First try to downcast to SdkError
    if let Some(sdk_err) = e.downcast_ref::<SdkError>() {
        return error_to_ntstatus(sdk_err);
    }
    // Try to downcast to std::io::Error
    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
        match io_err.kind() {
            std::io::ErrorKind::NotFound => return STATUS_OBJECT_NAME_NOT_FOUND,
            std::io::ErrorKind::PermissionDenied => return STATUS_ACCESS_DENIED,
            std::io::ErrorKind::AlreadyExists => return STATUS_OBJECT_NAME_COLLISION,
            std::io::ErrorKind::InvalidInput => return STATUS_INVALID_PARAMETER,
            std::io::ErrorKind::StorageFull => return STATUS_DISK_FULL,
            _ => {}
        }
    }
    STATUS_INVALID_PARAMETER
}

/// Convert Unix mode to Windows file attributes
fn mode_to_attributes(mode: u32) -> u32 {
    let mut attrs = FILE_ATTRIBUTE_NORMAL;

    // Check file type
    let file_type = mode & 0o170000;
    if file_type == 0o040000 {
        attrs = FILE_ATTRIBUTE_DIRECTORY;
    } else if file_type == 0o120000 {
        attrs |= FILE_ATTRIBUTE_REPARSE_POINT;
    }

    // Check if file is read-only (no write permission for owner)
    if (mode & 0o200) == 0 {
        attrs |= FILE_ATTRIBUTE_READONLY;
    }

    attrs
}

/// Convert Stats to FileInfo for WinFsp.
fn fill_file_info(stats: &Stats, file_info: &mut FileInfo) {
    file_info.file_attributes = mode_to_attributes(stats.mode);
    file_info.reparse_tag = if stats.is_symlink() { 1 } else { 0 };
    file_info.allocation_size = (((stats.size + 4095) / 4096) * 4096) as u64;
    file_info.file_size = stats.size as u64;
    // Convert Unix timestamps to Windows FILETIME
    const UNIX_EPOCH_DIFF: i64 = 11644473600;
    file_info.creation_time = ((stats.ctime + UNIX_EPOCH_DIFF) * 10_000_000) as u64;
    file_info.last_access_time = ((stats.atime + UNIX_EPOCH_DIFF) * 10_000_000) as u64;
    file_info.last_write_time = ((stats.mtime + UNIX_EPOCH_DIFF) * 10_000_000) as u64;
    file_info.change_time = file_info.last_write_time;
    file_info.index_number = stats.ino as u64;
    file_info.hard_links = 1;
    file_info.ea_size = 0;
}

/// Tracks an open file or directory handle
struct OpenFile {
    /// The file handle (None for directories)
    file: Option<BoxedFile>,
    /// The inode number of the opened file or directory
    ino: i64,
    /// Whether this is a directory
    is_dir: bool,
    /// Pending delete flag - file should be deleted on close
    delete_on_close: std::sync::atomic::AtomicBool,
    /// Path to the file (for deletion on close)
    path: String,
}

/// WinFsp filesystem adapter wrapping an AgentFS FileSystem.
pub struct AgentFSWinFsp {
    fs: Arc<Mutex<dyn FileSystem + Send>>,
    handle: Handle,
    open_files: Mutex<HashMap<u64, OpenFile>>,
    next_fh: AtomicU64,
    dir_buffer: DirBuffer,
}

impl AgentFSWinFsp {
    /// Create a new WinFsp filesystem adapter wrapping a FileSystem instance.
    pub fn new(fs: Arc<Mutex<dyn FileSystem + Send>>, handle: Handle) -> Self {
        Self {
            fs,
            handle,
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            dir_buffer: DirBuffer::new(),
        }
    }

    /// Execute an async future in a WinFsp sync callback safely.
    /// Uses block_in_place to allow blocking in an async context,
    /// then handle.block_on to run the future on the existing runtime.
    fn block_on<F: Future>(&self, f: F) -> F::Output {
        tokio::task::block_in_place(|| self.handle.block_on(f))
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }

    fn win_path_to_unix(path: &U16CStr) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    /// Parse a path into (parent_ino, name) for operations that need a parent directory.
    /// For multi-level paths, this will walk the path to find the parent directory.
    fn parse_path(&self, path: &str) -> Result<(i64, String)> {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return Ok((1, String::new()));
        }

        // Split path into components
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if components.is_empty() {
            return Ok((1, String::new()));
        }

        // The last component is the name, everything before is the path to parent
        let name = components.last().unwrap().to_string();
        if components.len() == 1 {
            // Direct child of root
            return Ok((1, name));
        }

        // Walk the path to find the parent directory
        let mut current_ino: i64 = 1;
        for component in &components[..components.len() - 1] {
            let fs = self.fs.clone();
            let component_owned = component.to_string();
            let result = self.block_on(async move {
                fs.lock().lookup(current_ino, &component_owned).await
            });

            match result {
                Ok(Some(stats)) => {
                    if stats.is_directory() {
                        current_ino = stats.ino;
                    } else {
                        return Err(anyhow::anyhow!("Not a directory"));
                    }
                }
                Ok(None) => return Err(anyhow::anyhow!("Path not found")),
                Err(e) => return Err(e.into()),
            }
        }

        Ok((current_ino, name))
    }

    /// Look up a path and return its stats. Walks the entire path.
    fn path_lookup(&self, path: &str) -> Result<Option<Stats>> {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            let fs = self.fs.clone();
            return Ok(self.block_on(async move {
                fs.lock().getattr(1).await
            })?);
        }

        let (parent_ino, name) = self.parse_path(path)?;
        let fs = self.fs.clone();
        Ok(self.block_on(async move {
            fs.lock().lookup(parent_ino, &name).await
        })?)
    }
}

/// File context for WinFsp - represents an open file handle.
pub struct FileContext {
    fh: u64,
}

impl FileSystemContext for AgentFSWinFsp {
    type FileContext = FileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = Self::win_path_to_unix(file_name);

        tracing::debug!("WinFsp::get_security_by_name: {}", path);

        match self.path_lookup(&path) {
            Ok(Some(stats)) => {
                let is_symlink = stats.is_symlink();
                Ok(FileSecurity {
                    // Set reparse=true for symlinks so Windows knows to follow them
                    reparse: is_symlink,
                    attributes: mode_to_attributes(stats.mode),
                    sz_security_descriptor: 0,
                })
            }
            Ok(None) => Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND)),
            Err(e) => Err(FspError::NTSTATUS(anyhow_to_ntstatus(&e))),
        }
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = Self::win_path_to_unix(file_name);

        tracing::debug!("WinFsp::open: {}", path);

        match self.path_lookup(&path) {
            Ok(Some(stats)) => {
                fill_file_info(&stats, file_info.as_mut());

                let fh = self.alloc_fh();
                let is_dir = stats.is_directory();
                let path_owned = path.clone();

                if is_dir {
                    // For directories, we don't need a file handle, just track the inode
                    self.open_files.lock().insert(fh, OpenFile {
                        file: None,
                        ino: stats.ino,
                        is_dir: true,
                        delete_on_close: std::sync::atomic::AtomicBool::new(false),
                        path: path_owned,
                    });
                    Ok(FileContext { fh })
                } else {
                    // For files, open the file handle
                    let fs = self.fs.clone();
                    let ino = stats.ino;
                    let file = self.block_on(async move {
                        fs.lock().open(ino, 0).await
                    });

                    match file {
                        Ok(file) => {
                            self.open_files.lock().insert(fh, OpenFile {
                                file: Some(file),
                                ino: stats.ino,
                                is_dir: false,
                                delete_on_close: std::sync::atomic::AtomicBool::new(false),
                                path: path_owned,
                            });
                            Ok(FileContext { fh })
                        }
                        Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
                    }
                }
            }
            Ok(None) => Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND)),
            Err(e) => Err(FspError::NTSTATUS(anyhow_to_ntstatus(&e))),
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = Self::win_path_to_unix(file_name);
        let is_dir = (create_options & FILE_DIRECTORY_FILE) != 0;

        tracing::debug!("WinFsp::create: {} (is_dir={})", path, is_dir);

        // Parse path to get parent_ino and name
        let (parent_ino, name) = self.parse_path(&path)
            .map_err(|e| FspError::NTSTATUS(anyhow_to_ntstatus(&e)))?;

        // First, check if the file already exists
        let existing = {
            let fs = self.fs.clone();
            let name_clone = name.clone();
            self.block_on(async move {
                fs.lock().lookup(parent_ino, &name_clone).await
            })
        };

        match existing {
            Ok(Some(stats)) => {
                // File already exists - open it directly
                // WinFsp will call overwrite() if truncation is needed
                tracing::debug!("WinFsp::create: file exists, opening {} (ino={})", path, stats.ino);
                fill_file_info(&stats, file_info.as_mut());

                let fh = self.alloc_fh();
                let path_owned = path.clone();

                if is_dir {
                    self.open_files.lock().insert(fh, OpenFile {
                        file: None,
                        ino: stats.ino,
                        is_dir: true,
                        delete_on_close: std::sync::atomic::AtomicBool::new(false),
                        path: path_owned,
                    });
                    Ok(FileContext { fh })
                } else {
                    let fs = self.fs.clone();
                    let ino = stats.ino;
                    let file = self.block_on(async move {
                        fs.lock().open(ino, 0).await
                    });

                    match file {
                        Ok(file) => {
                            self.open_files.lock().insert(fh, OpenFile {
                                file: Some(file),
                                ino: stats.ino,
                                is_dir: false,
                                delete_on_close: std::sync::atomic::AtomicBool::new(false),
                                path: path_owned,
                            });
                            Ok(FileContext { fh })
                        }
                        Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
                    }
                }
            }
            Ok(None) => {
                // File does not exist - create it
                tracing::debug!("WinFsp::create: file does not exist, creating {}", path);

                let fs = self.fs.clone();
                let name_owned = name.clone();

                // Use uid=0, gid=0 for now (root user)
                // TODO: Get actual user context from Windows
                let uid = 0u32;
                let gid = 0u32;

                let result = if is_dir {
                    // Create directory with mode 0755 (rwxr-xr-x)
                    self.block_on(async move {
                        fs.lock().mkdir(parent_ino, &name_owned, 0o755, uid, gid).await
                    })
                } else {
                    // Create file with mode 0644 (rw-r--r--)
                    self.block_on(async move {
                        fs.lock().mknod(parent_ino, &name_owned, 0o100644, 0, uid, gid).await
                    })
                };

                match result {
                    Ok(stats) => {
                        fill_file_info(&stats, file_info.as_mut());

                        let fh = self.alloc_fh();
                        let path_owned = path.clone();

                        if is_dir {
                            // For directories, we don't need a file handle
                            self.open_files.lock().insert(fh, OpenFile {
                                file: None,
                                ino: stats.ino,
                                is_dir: true,
                                delete_on_close: std::sync::atomic::AtomicBool::new(false),
                                path: path_owned,
                            });
                            Ok(FileContext { fh })
                        } else {
                            // For files, open the newly created file
                            let fs = self.fs.clone();
                            let ino = stats.ino;
                            let file = self.block_on(async move {
                                fs.lock().open(ino, 0).await
                            });

                            match file {
                                Ok(file) => {
                                    self.open_files.lock().insert(fh, OpenFile {
                                        file: Some(file),
                                        ino: stats.ino,
                                        is_dir: false,
                                        delete_on_close: std::sync::atomic::AtomicBool::new(false),
                                        path: path_owned,
                                    });
                                    Ok(FileContext { fh })
                                }
                                Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
                            }
                        }
                    }
                    Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
                }
            }
            Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
        }
    }

    fn close(&self, context: Self::FileContext) {
        // Check if this file/directory is marked for deletion
        let open_file = self.open_files.lock().remove(&context.fh);
        if let Some(open_file) = open_file {
            if open_file.delete_on_close.load(std::sync::atomic::Ordering::SeqCst) {
                // Delete the file/directory now using the stored path
                let path = open_file.path;
                let is_dir = open_file.is_dir;

                // Parse path to get parent_ino and name
                match self.parse_path(&path) {
                    Ok((parent_ino, name)) => {
                        let fs = self.fs.clone();
                        let result = self.block_on(async move {
                            let fs_guard = fs.lock();
                            if is_dir {
                                fs_guard.rmdir(parent_ino, &name).await
                            } else {
                                fs_guard.unlink(parent_ino, &name).await
                            }
                        });

                        if let Err(e) = result {
                            tracing::warn!("Failed to delete {} on close: {}", path, e);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse path for deletion: {}", e);
                    }
                }
            }
        }
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let open_files = self.open_files.lock();
        if let Some(open_file) = open_files.get(&context.fh) {
            let ino = open_file.ino;
            drop(open_files);

            let fs = self.fs.clone();
            let stats = self.block_on(async move {
                fs.lock().getattr(ino).await
            });

            match stats {
                Ok(Some(stats)) => {
                    fill_file_info(&stats, file_info);
                    Ok(())
                }
                Ok(None) => Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND)),
                Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
            }
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER))
        }
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        _security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        Ok(0)
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let open_files = self.open_files.lock();
        if let Some(open_file) = open_files.get(&context.fh) {
            let ino = open_file.ino;
            drop(open_files);

            let atime = if last_access_time == 0 {
                TimeChange::Omit
            } else {
                TimeChange::Set((last_access_time as i64 / 10_000_000) - 11644473600, 0)
            };
            let mtime = if last_write_time == 0 {
                TimeChange::Omit
            } else {
                TimeChange::Set((last_write_time as i64 / 10_000_000) - 11644473600, 0)
            };

            let fs = self.fs.clone();
            let result = self.block_on(async move {
                fs.lock().utimens(ino, atime, mtime).await
            });

            match result {
                Ok(()) => {
                    let fs = self.fs.clone();
                    let stats = self.block_on(async move {
                        fs.lock().getattr(ino).await
                    });
                    match stats {
                        Ok(Some(stats)) => {
                            fill_file_info(&stats, file_info);
                            Ok(())
                        }
                        _ => Ok(()),
                    }
                }
                Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
            }
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER))
        }
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        // Mark the file for deletion on close (Unix unlink semantics)
        // The actual deletion happens in close()
        let open_files = self.open_files.lock();
        if let Some(open_file) = open_files.get(&context.fh) {
            open_file.delete_on_close.store(
                delete_file,
                std::sync::atomic::Ordering::SeqCst,
            );
            Ok(())
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER))
        }
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let old_path = Self::win_path_to_unix(file_name);
        let new_path = Self::win_path_to_unix(new_file_name);
        let (old_parent, old_name) = self.parse_path(&old_path)
            .map_err(|e| FspError::NTSTATUS(anyhow_to_ntstatus(&e)))?;
        let (new_parent, new_name) = self.parse_path(&new_path)
            .map_err(|e| FspError::NTSTATUS(anyhow_to_ntstatus(&e)))?;

        let fs = self.fs.clone();
        let result = self.block_on(async move {
            fs.lock().rename(old_parent, &old_name, new_parent, &new_name).await
        });

        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
        }
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: winfsp::filesystem::DirMarker<'_>,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        // Get the directory inode from the open file handle
        let dir_ino = {
            let open_files = self.open_files.lock();
            match open_files.get(&context.fh) {
                Some(open_file) => open_file.ino,
                None => return Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER)),
            }
        };

        let fs = self.fs.clone();
        let entries = self.block_on(async move {
            fs.lock().readdir_plus(dir_ino).await
        });

        match entries {
            Ok(Some(entries)) => {
                // Determine starting index based on marker.
                // The marker is the filename (U16CStr) of the last entry returned
                // in the previous call. We need to find it and skip past it.
                let start_idx = if let Some(marker_name) = marker.inner_as_cstr() {
                    let marker_str = marker_name.to_string_lossy();
                    let mut idx = 0usize;
                    for (i, entry) in entries.iter().enumerate() {
                        if entry.name == marker_str {
                            idx = i + 1;
                            break;
                        }
                    }
                    idx
                } else {
                    0
                };
                let mut cursor = 0u32;

                for entry in entries.iter().skip(start_idx) {
                    let mut dir_info: DirInfo<255> = DirInfo::default();
                    fill_file_info(&entry.stats, dir_info.file_info_mut());

                    // Set the name using WideNameInfo trait
                    if dir_info.set_name(&entry.name).is_ok() {
                        // Use the proper WinFsp API to append to buffer
                        // This handles variable-length entries correctly
                        if !dir_info.append_to_buffer(buffer, &mut cursor) {
                            // Buffer is full, stop adding more entries
                            break;
                        }
                    }
                }

                // Finalize the buffer
                DirInfo::<255>::finalize_buffer(buffer, &mut cursor);

                Ok(cursor)
            }
            Ok(None) => Err(FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND)),
            Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
        }
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        let open_files = self.open_files.lock();
        if let Some(open_file) = open_files.get(&context.fh) {
            let file = open_file.file.clone();
            let buf_len = buffer.len();
            drop(open_files);

            // file is Option<BoxedFile>, need to handle None case
            let file = match file {
                Some(f) => f,
                None => return Err(FspError::NTSTATUS(STATUS_FILE_IS_A_DIRECTORY)),
            };

            let result = self.block_on(async move {
                file.pread(offset, buf_len as u64).await
            });

            match result {
                Ok(data) => {
                    let len = data.len().min(buffer.len());
                    buffer[..len].copy_from_slice(&data[..len]);
                    Ok(len as u32)
                }
                Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
            }
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER))
        }
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        _write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let open_files = self.open_files.lock();
        if let Some(open_file) = open_files.get(&context.fh) {
            let file = open_file.file.clone();
            let ino = open_file.ino;
            drop(open_files);

            // file is Option<BoxedFile>, need to handle None case
            let file = match file {
                Some(f) => f,
                None => return Err(FspError::NTSTATUS(STATUS_FILE_IS_A_DIRECTORY)),
            };

            let result = self.block_on(async move {
                file.pwrite(offset, buffer).await
            });

            match result {
                Ok(()) => {
                    let fs = self.fs.clone();
                    let stats = self.block_on(async move {
                        fs.lock().getattr(ino).await
                    });
                    if let Ok(Some(stats)) = stats {
                        fill_file_info(&stats, file_info);
                    }
                    Ok(buffer.len() as u32)
                }
                Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
            }
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER))
        }
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let open_files = self.open_files.lock();
        if let Some(open_file) = open_files.get(&context.fh) {
            // For directories, just return success (no-op)
            if open_file.is_dir {
                return Ok(());
            }

            let file = open_file.file.clone();
            let ino = open_file.ino;
            drop(open_files);

            let file = match file {
                Some(f) => f,
                None => return Ok(()), // Should not happen, but handle gracefully
            };

            if set_allocation_size {
                // allocation_size: pre-allocate space
                // For simplicity, we treat this as a no-op since SQLite handles allocation
                // Just update the file info
                let fs = self.fs.clone();
                if let Ok(Some(stats)) = self.block_on(async move {
                    fs.lock().getattr(ino).await
                }) {
                    fill_file_info(&stats, file_info);
                }
                Ok(())
            } else {
                // file_size: truncate or extend the file
                let result = self.block_on(async move {
                    file.truncate(new_size).await
                });

                match result {
                    Ok(()) => {
                        let fs = self.fs.clone();
                        if let Ok(Some(stats)) = self.block_on(async move {
                            fs.lock().getattr(ino).await
                        }) {
                            fill_file_info(&stats, file_info);
                        }
                        Ok(())
                    }
                    Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
                }
            }
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER))
        }
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        // Handle overwrite: called when creating file with FILE_SUPERSEDE or FILE_OVERWRITE
        // Truncate the file to zero length
        let open_files = self.open_files.lock();
        if let Some(open_file) = open_files.get(&context.fh) {
            if open_file.is_dir {
                return Ok(());
            }

            let file = open_file.file.clone();
            let ino = open_file.ino;
            drop(open_files);

            let file = match file {
                Some(f) => f,
                None => return Ok(()),
            };

            // Truncate to zero
            let result = self.block_on(async move {
                file.truncate(0).await
            });

            match result {
                Ok(()) => {
                    let fs = self.fs.clone();
                    if let Ok(Some(stats)) = self.block_on(async move {
                        fs.lock().getattr(ino).await
                    }) {
                        fill_file_info(&stats, file_info);
                    }
                    Ok(())
                }
                Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
            }
        } else {
            Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER))
        }
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if let Some(context) = context {
            let open_files = self.open_files.lock();
            if let Some(open_file) = open_files.get(&context.fh) {
                let file = open_file.file.clone();
                let ino = open_file.ino;
                drop(open_files);

                // file is Option<BoxedFile>, directories don't need flush
                if let Some(file) = file {
                    let result = self.block_on(async move {
                        file.fsync().await
                    });

                    match result {
                        Ok(()) => {
                            let fs = self.fs.clone();
                            let stats = self.block_on(async move {
                                fs.lock().getattr(ino).await
                            });
                            if let Ok(Some(stats)) = stats {
                                fill_file_info(&stats, file_info);
                            }
                            Ok(())
                        }
                        Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
                    }
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let fs = self.fs.clone();
        let stats = self.block_on(async move {
            fs.lock().statfs().await
        });

        match stats {
            Ok(stats) => {
                out_volume_info.total_size = 1024 * 1024 * 1024; // 1GB
                out_volume_info.free_size = 1024 * 1024 * 1024 - stats.bytes_used;
                Ok(())
            }
            Err(e) => Err(FspError::NTSTATUS(error_to_ntstatus(&e))),
        }
    }
}

/// Mount options for WinFsp.
pub struct MountOpts {
    pub mountpoint: PathBuf,
    pub fsname: String,
}

/// Mount an AgentFS filesystem using WinFsp.
pub fn mount(
    fs: Arc<Mutex<dyn FileSystem + Send>>,
    opts: MountOpts,
) -> Result<()> {
    let mountpoint = opts.mountpoint.clone();
    // Try to get the current runtime handle, or create a new runtime if not in async context
    let handle = match Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            // Not in an async context, need to use a different approach
            // Since this is a sync function without a runtime, we can't really work
            // Let the caller know they need to call this from an async context
            anyhow::bail!("mount() must be called from within a tokio runtime context");
        }
    };
    let adapter = AgentFSWinFsp::new(fs, handle);

    let mut volume_params = winfsp::host::VolumeParams::default();
    volume_params.case_sensitive_search(true);
    volume_params.filesystem_name(&opts.fsname);

    let mut host = winfsp::host::FileSystemHost::new(volume_params, adapter)?;

    let mountpoint_str = mountpoint.to_string_lossy().to_string();
    tracing::info!("Mounting WinFsp filesystem at {}", mountpoint_str);

    host.mount(mountpoint_str.as_str())?;
    // Note: WinFsp doesn't have a run() method in the traditional sense.
    // The mount() call blocks until the host is dropped
    // The filesystem will continue to operate until the host is dropped
    // For async operation, the mount() returns immediately, so caller needs to
    // keep the host alive if they want to stop the filesystem
    Ok(())
}

/// Unmount a WinFsp filesystem.
pub fn unmount(_mountpoint: &std::path::Path, _lazy: bool) -> Result<()> {
    Ok(())
}
