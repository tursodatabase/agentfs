//! `.fsignore` support for filtering files from the HostFS passthrough layer.
//!
//! This module provides `.gitignore`-compatible pattern matching via the `ignore`
//! crate. When a `.fsignore` file exists in the HostFS root directory, matching
//! files and directories are hidden from `lookup`, `readdir`, and `readdir_plus`
//! operations — as if they don't exist on the host filesystem.
//!
//! The `.fsignore` file uses the same syntax as `.gitignore`:
//! - One pattern per line
//! - `#` for comments
//! - `!` prefix for negation (un-ignore)
//! - Trailing `/` to match directories only
//! - `*`, `**`, `?` glob wildcards
//!
//! The `.fsignore` file itself is never ignored.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;
use tracing::debug;

/// The filename used for ignore patterns.
pub const FSIGNORE_FILE: &str = ".fsignore";

/// Compiled ignore patterns loaded from a `.fsignore` file.
///
/// This is cheaply cloneable (wraps an `Arc` internally in the `ignore` crate).
#[derive(Clone)]
pub struct FsIgnore {
    matcher: Gitignore,
}

impl FsIgnore {
    /// Load ignore patterns from `<root>/.fsignore`.
    ///
    /// If the file does not exist or cannot be read, returns an `FsIgnore` that
    /// matches nothing (i.e. no files are ignored).
    pub fn load(root: &Path) -> Self {
        let fsignore_path = root.join(FSIGNORE_FILE);

        let mut builder = GitignoreBuilder::new(root);

        if fsignore_path.is_file() {
            if let Some(err) = builder.add(&fsignore_path) {
                debug!(
                    "Warning: failed to parse {}: {}",
                    fsignore_path.display(),
                    err
                );
            }
        }

        let matcher = builder.build().unwrap_or_else(|e| {
            debug!("Failed to build ignore matcher: {}", e);
            Gitignore::empty()
        });

        Self { matcher }
    }

    /// Check whether a path should be ignored.
    ///
    /// `path` must be an absolute path to the file or directory on the host
    /// filesystem. `is_dir` indicates whether the path is a directory (needed
    /// for patterns that end with `/`).
    ///
    /// Returns `true` if the path matches an ignore pattern (and is not
    /// negated by a later `!` pattern).
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        // Never ignore the .fsignore file itself.
        if path.file_name().and_then(|n| n.to_str()) == Some(FSIGNORE_FILE) {
            // Only protect the root-level .fsignore, not nested ones with the
            // same name (which would be unusual but possible).
            if path.parent() == self.matcher.path() {
                return false;
            }
        }

        self.matcher
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
    }

    /// Returns `true` if no ignore patterns are loaded (the file was absent or
    /// empty). Useful for short-circuiting checks.
    pub fn is_empty(&self) -> bool {
        self.matcher.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_no_fsignore_file() {
        let dir = tempdir().unwrap();
        let ignore = FsIgnore::load(dir.path());
        assert!(ignore.is_empty());
        // Nothing should be ignored.
        assert!(!ignore.is_ignored(&dir.path().join("anything.txt"), false));
    }

    #[test]
    fn test_basic_file_pattern() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(FSIGNORE_FILE), "*.log\n").unwrap();

        let ignore = FsIgnore::load(dir.path());
        assert!(!ignore.is_empty());
        assert!(ignore.is_ignored(&dir.path().join("debug.log"), false));
        assert!(!ignore.is_ignored(&dir.path().join("readme.md"), false));
    }

    #[test]
    fn test_directory_pattern() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(FSIGNORE_FILE), "target/\nnode_modules/\n").unwrap();

        let ignore = FsIgnore::load(dir.path());
        assert!(ignore.is_ignored(&dir.path().join("target"), true));
        assert!(ignore.is_ignored(&dir.path().join("node_modules"), true));
        // A file named "target" (not a directory) should not match "target/"
        assert!(!ignore.is_ignored(&dir.path().join("target"), false));
    }

    #[test]
    fn test_nested_path() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(FSIGNORE_FILE), "target/\n").unwrap();

        let ignore = FsIgnore::load(dir.path());
        // Files inside an ignored directory should be ignored.
        assert!(ignore.is_ignored(&dir.path().join("target").join("debug").join("foo"), false));
    }

    #[test]
    fn test_negation() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(FSIGNORE_FILE),
            "*.bin\n!important.bin\n",
        )
        .unwrap();

        let ignore = FsIgnore::load(dir.path());
        assert!(ignore.is_ignored(&dir.path().join("data.bin"), false));
        assert!(!ignore.is_ignored(&dir.path().join("important.bin"), false));
    }

    #[test]
    fn test_comments_and_blank_lines() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(FSIGNORE_FILE),
            "# This is a comment\n\n*.tmp\n\n# Another comment\n",
        )
        .unwrap();

        let ignore = FsIgnore::load(dir.path());
        assert!(ignore.is_ignored(&dir.path().join("scratch.tmp"), false));
        assert!(!ignore.is_ignored(&dir.path().join("readme.md"), false));
    }

    #[test]
    fn test_fsignore_file_itself_not_ignored() {
        let dir = tempdir().unwrap();
        // Even if a wildcard matches .fsignore, it should not be hidden.
        fs::write(dir.path().join(FSIGNORE_FILE), ".*\n").unwrap();

        let ignore = FsIgnore::load(dir.path());
        assert!(
            !ignore.is_ignored(&dir.path().join(FSIGNORE_FILE), false),
            ".fsignore at root should never be ignored"
        );
        // Other dotfiles should still be ignored.
        assert!(ignore.is_ignored(&dir.path().join(".env"), false));
    }

    #[test]
    fn test_double_star_pattern() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(FSIGNORE_FILE), "**/build/\n").unwrap();

        let ignore = FsIgnore::load(dir.path());
        assert!(ignore.is_ignored(&dir.path().join("build"), true));
        assert!(ignore.is_ignored(&dir.path().join("src").join("build"), true));
    }

    #[test]
    fn test_multiple_patterns() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(FSIGNORE_FILE),
            ".git/\nnode_modules/\ntarget/\n*.pyc\n__pycache__/\n",
        )
        .unwrap();

        let ignore = FsIgnore::load(dir.path());
        assert!(ignore.is_ignored(&dir.path().join(".git"), true));
        assert!(ignore.is_ignored(&dir.path().join("node_modules"), true));
        assert!(ignore.is_ignored(&dir.path().join("target"), true));
        assert!(ignore.is_ignored(&dir.path().join("main.pyc"), false));
        assert!(ignore.is_ignored(&dir.path().join("__pycache__"), true));
        assert!(!ignore.is_ignored(&dir.path().join("src"), true));
        assert!(!ignore.is_ignored(&dir.path().join("main.rs"), false));
    }
}
