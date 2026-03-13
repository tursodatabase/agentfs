# Windows Mounting Test Script

This PowerShell script provides comprehensive testing for AgentFS Windows mounting functionality.

## Quick Start

```powershell
# Run complete test suite
.\test-windows-mount.ps1

# Run quick test (core features only)
.\test-windows-mount.ps1 -QuickTest

# Run specific module
.\test-windows-mount.ps1 -TestModule "BasicFileOperations"

# Keep mount after testing (for manual inspection)
.\test-windows-mount.ps1 -SkipCleanup
```

## Parameters

- `-AgentId`: Agent ID to use for testing (default: "win-test")
- `-MountPoint`: Mount point directory (default: "C:\mnt\win-test")
- `-QuickTest`: Run only core functionality tests
- `-TestModule`: Run specific test module only
- `-SkipCleanup`: Don't cleanup after tests (keep mount and database)

## Test Modules

### Module A: Environment Setup
- Initialize agent database
- Create mount point
- Start mount process
- Verify mount is accessible

### Module B: Basic File Operations
- Create, read, write, append files
- Binary file operations
- Empty file creation
- File deletion

### Module C: Directory Operations
- Create and delete directories
- Nested directories
- Multi-level directory creation
- Directory renaming

### Module D: Symbolic Links
**Requires Administrator privileges**
- Create file symbolic links
- Create directory symbolic links
- Read through symlinks
- Delete symlinks

### Module E: File Attributes
- File size verification
- Timestamp checks (creation, modification, access)
- File existence tests
- Directory vs file identification

### Module F: Edge Cases
- Large files (>4KB for chunking)
- Long filenames
- Special characters (Chinese, spaces)
- Various filename patterns

### Module G: Error Handling
- Read non-existent files
- Delete non-existent files
- Create files in non-existent directories
- Invalid filenames

### Module H: AgentFS CLI Commands
- `agentfs fs ls` command
- `agentfs fs cat` command
- `agentfs fs write` command
- `agentfs timeline` command

### Module I: Windows-Specific Tests
- Windows path separators (backslash and forward slash)
- PowerShell cmdlets (Out-File, Get-Content, Copy-Item, Move-Item)
- Windows-specific operations

### Module J: Performance Tests
- Create 100 small files
- List large directories
- Read/write 10MB files

## Requirements

- PowerShell 5.1 or PowerShell Core 6.0+
- AgentFS CLI built and available via `cargo run`
- For symbolic link tests: Administrator privileges

## Output

The script provides:
- Real-time colored output (✓ green for pass, ✗ red for fail)
- Detailed error messages for failed tests
- Test summary with pass rate
- Total execution time

## Example Output

```
================================================
  AgentFS Windows Mounting Test Suite
  Agent ID: win-test
  Mount Point: C:\mnt\win-test
================================================

========== Module A: Environment Setup ==========
✓ Initialize agent database
✓ Database file created at .agentfs/win-test.db
✓ Mount point directory created at C:\mnt\win-test
...

================================================
  Test Summary
================================================
Total Duration: 12.34 seconds
Tests Passed:   45
Tests Failed:   0
Tests Skipped:  6
================================================
Pass Rate: 88.24%
================================================
```

## Troubleshooting

### Mount doesn't start
- Check if mount point directory exists
- Check if agentfs CLI is properly built
- Check mount-stdout.log and mount-stderr.log for errors

### Symbolic link tests fail
- Run PowerShell as Administrator
- Check if Developer Mode is enabled in Windows

### Performance tests fail
- Ensure sufficient disk space
- Check if antivirus is interfering

### Cleanup fails
- Manually unmount: Kill the cargo process
- Manually delete: Remove .agentfs directory
- Check for locked files in mount point

## Manual Testing

After running tests with `-SkipCleanup`, you can manually test:

```powershell
# Navigate to mount point
cd C:\mnt\win-test

# Create files
echo "test" > test.txt

# List files
dir

# Use AgentFS CLI
cargo run -- fs ls .agentfs\win-test.db
```

## Test Coverage

This script tests all major AgentFS features documented in SPEC.md:
- Virtual Filesystem (fs_inode, fs_dentry, fs_data, fs_symlink)
- File operations (create, read, write, delete)
- Directory operations
- Symbolic links
- File metadata and attributes
- Large file handling (chunked storage)
- Error conditions

For OverlayFS testing, you'll need to create a separate test with the `--base` option.
