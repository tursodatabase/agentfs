package agentfs

import (
	"context"
	"errors"
	"testing"
)

func setupSymlinkTest(t *testing.T) (*Filesystem, context.Context) {
	t.Helper()
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{Path: ":memory:"})
	if err != nil {
		t.Fatalf("Failed to open AgentFS: %v", err)
	}
	t.Cleanup(func() { afs.Close() })
	return afs.FS, ctx
}

func TestSymlinkResolution(t *testing.T) {
	t.Run("Stat follows final symlink", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.WriteFile(ctx, "/target.txt", []byte("hello"), 0o644)
		fs.Symlink(ctx, "/target.txt", "/link.txt")

		stats, err := fs.Stat(ctx, "/link.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsRegularFile() {
			t.Error("Stat on symlink should follow to target (regular file)")
		}
		if stats.Size != 5 {
			t.Errorf("Size = %d, want 5", stats.Size)
		}
	})

	t.Run("Lstat does not follow final symlink", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.WriteFile(ctx, "/target.txt", []byte("hello"), 0o644)
		fs.Symlink(ctx, "/target.txt", "/link.txt")

		stats, err := fs.Lstat(ctx, "/link.txt")
		if err != nil {
			t.Fatalf("Lstat failed: %v", err)
		}
		if !stats.IsSymlink() {
			t.Error("Lstat should return symlink stats")
		}
	})

	t.Run("Lstat and Stat identical for non-symlinks", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.WriteFile(ctx, "/file.txt", []byte("data"), 0o644)

		stat, err := fs.Stat(ctx, "/file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		lstat, err := fs.Lstat(ctx, "/file.txt")
		if err != nil {
			t.Fatalf("Lstat failed: %v", err)
		}
		if stat.Ino != lstat.Ino {
			t.Errorf("Stat.Ino=%d != Lstat.Ino=%d", stat.Ino, lstat.Ino)
		}
		if stat.Mode != lstat.Mode {
			t.Errorf("Stat.Mode=%o != Lstat.Mode=%o", stat.Mode, lstat.Mode)
		}
	})

	t.Run("intermediate symlink traversal", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.Mkdir(ctx, "/realdir", 0o755)
		fs.WriteFile(ctx, "/realdir/file.txt", []byte("found"), 0o644)
		fs.Symlink(ctx, "/realdir", "/dirlink")

		// Access file through the symlinked directory
		data, err := fs.ReadFile(ctx, "/dirlink/file.txt")
		if err != nil {
			t.Fatalf("ReadFile through symlinked dir failed: %v", err)
		}
		if string(data) != "found" {
			t.Errorf("Content = %q, want %q", string(data), "found")
		}

		// Stat through symlinked directory
		stats, err := fs.Stat(ctx, "/dirlink/file.txt")
		if err != nil {
			t.Fatalf("Stat through symlinked dir failed: %v", err)
		}
		if !stats.IsRegularFile() {
			t.Error("Expected regular file through symlinked dir")
		}
	})

	t.Run("ELOOP circular symlinks", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.Symlink(ctx, "/b", "/a")
		fs.Symlink(ctx, "/a", "/b")

		_, err := fs.Stat(ctx, "/a")
		if err == nil {
			t.Fatal("Expected ELOOP error")
		}
		if !IsLoop(err) {
			t.Errorf("Expected ELOOP, got: %v", err)
		}
	})

	t.Run("ELOOP self-referencing symlink", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.Symlink(ctx, "/self", "/self")

		_, err := fs.Stat(ctx, "/self")
		if err == nil {
			t.Fatal("Expected ELOOP error")
		}
		if !IsLoop(err) {
			t.Errorf("Expected ELOOP, got: %v", err)
		}
	})

	t.Run("chain of symlinks", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.WriteFile(ctx, "/target.txt", []byte("end"), 0o644)
		fs.Symlink(ctx, "/target.txt", "/c")
		fs.Symlink(ctx, "/c", "/b")
		fs.Symlink(ctx, "/b", "/a")

		// Stat follows the chain a -> b -> c -> target.txt
		stats, err := fs.Stat(ctx, "/a")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsRegularFile() {
			t.Error("Expected regular file after following chain")
		}

		data, err := fs.ReadFile(ctx, "/a")
		if err != nil {
			t.Fatalf("ReadFile through chain failed: %v", err)
		}
		if string(data) != "end" {
			t.Errorf("Content = %q, want %q", string(data), "end")
		}
	})

	t.Run("relative symlink resolution", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.Mkdir(ctx, "/dir", 0o755)
		fs.WriteFile(ctx, "/dir/target.txt", []byte("relative"), 0o644)
		// Relative symlink in /dir pointing to sibling file
		fs.Symlink(ctx, "target.txt", "/dir/link.txt")

		data, err := fs.ReadFile(ctx, "/dir/link.txt")
		if err != nil {
			t.Fatalf("ReadFile through relative symlink failed: %v", err)
		}
		if string(data) != "relative" {
			t.Errorf("Content = %q, want %q", string(data), "relative")
		}
	})

	t.Run("relative symlink with dotdot", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.WriteFile(ctx, "/root_file.txt", []byte("from root"), 0o644)
		fs.Mkdir(ctx, "/subdir", 0o755)
		// Symlink in /subdir pointing to ../root_file.txt
		fs.Symlink(ctx, "../root_file.txt", "/subdir/up_link.txt")

		data, err := fs.ReadFile(ctx, "/subdir/up_link.txt")
		if err != nil {
			t.Fatalf("ReadFile through relative dotdot symlink failed: %v", err)
		}
		if string(data) != "from root" {
			t.Errorf("Content = %q, want %q", string(data), "from root")
		}
	})

	t.Run("dangling symlink: Stat returns ENOENT, Lstat succeeds", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.Symlink(ctx, "/nonexistent", "/dangling")

		_, err := fs.Stat(ctx, "/dangling")
		if err == nil {
			t.Fatal("Expected error from Stat on dangling symlink")
		}
		if !IsNotExist(err) {
			t.Errorf("Expected ENOENT, got: %v", err)
		}

		stats, err := fs.Lstat(ctx, "/dangling")
		if err != nil {
			t.Fatalf("Lstat on dangling symlink should succeed: %v", err)
		}
		if !stats.IsSymlink() {
			t.Error("Lstat should show it's a symlink")
		}
	})

	t.Run("operations through symlinked directories", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.Mkdir(ctx, "/real", 0o755)
		fs.Symlink(ctx, "/real", "/linked")

		// Mkdir through symlink
		err := fs.Mkdir(ctx, "/linked/subdir", 0o755)
		if err != nil {
			t.Fatalf("Mkdir through symlink failed: %v", err)
		}

		// Write through symlink
		err = fs.WriteFile(ctx, "/linked/subdir/file.txt", []byte("through link"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile through symlink failed: %v", err)
		}

		// Read through original path
		data, err := fs.ReadFile(ctx, "/real/subdir/file.txt")
		if err != nil {
			t.Fatalf("ReadFile from real path failed: %v", err)
		}
		if string(data) != "through link" {
			t.Errorf("Content = %q, want %q", string(data), "through link")
		}

		// Unlink through symlink
		err = fs.Unlink(ctx, "/linked/subdir/file.txt")
		if err != nil {
			t.Fatalf("Unlink through symlink failed: %v", err)
		}

		// Verify gone from real path
		_, err = fs.Stat(ctx, "/real/subdir/file.txt")
		if !IsNotExist(err) {
			t.Error("File should be gone from real path")
		}
	})

	t.Run("Readlink follows intermediate symlinks", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.Mkdir(ctx, "/real", 0o755)
		fs.Symlink(ctx, "/somewhere", "/real/inner_link")
		fs.Symlink(ctx, "/real", "/dir_link")

		// Readlink /dir_link/inner_link should follow dir_link but not inner_link
		target, err := fs.Readlink(ctx, "/dir_link/inner_link")
		if err != nil {
			t.Fatalf("Readlink failed: %v", err)
		}
		if target != "/somewhere" {
			t.Errorf("Target = %q, want %q", target, "/somewhere")
		}
	})

	t.Run("Open follows symlinks", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.WriteFile(ctx, "/openme.txt", []byte("opened"), 0o644)
		fs.Symlink(ctx, "/openme.txt", "/open_link.txt")

		f, err := fs.Open(ctx, "/open_link.txt", O_RDONLY)
		if err != nil {
			t.Fatalf("Open through symlink failed: %v", err)
		}

		buf := make([]byte, 64)
		n, _ := f.Read(buf)
		if string(buf[:n]) != "opened" {
			t.Errorf("Content = %q, want %q", string(buf[:n]), "opened")
		}
	})

	t.Run("ENOTDIR when intermediate is a regular file", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.WriteFile(ctx, "/notadir", []byte("x"), 0o644)

		_, err := fs.Stat(ctx, "/notadir/child")
		if err == nil {
			t.Fatal("Expected ENOTDIR error")
		}
		var fsErr *FSError
		if !errors.As(err, &fsErr) || fsErr.Code != ENOTDIR {
			t.Errorf("Expected ENOTDIR, got: %v", err)
		}
	})

	t.Run("Readdir through symlinked directory", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.Mkdir(ctx, "/dir", 0o755)
		fs.WriteFile(ctx, "/dir/a.txt", []byte("a"), 0o644)
		fs.WriteFile(ctx, "/dir/b.txt", []byte("b"), 0o644)
		fs.Symlink(ctx, "/dir", "/dir_link")

		entries, err := fs.Readdir(ctx, "/dir_link")
		if err != nil {
			t.Fatalf("Readdir through symlink failed: %v", err)
		}
		if len(entries) != 2 {
			t.Errorf("Expected 2 entries, got %d", len(entries))
		}
	})

	t.Run("Chmod follows symlink", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.WriteFile(ctx, "/chmod_target.txt", []byte("x"), 0o644)
		fs.Symlink(ctx, "/chmod_target.txt", "/chmod_link")

		err := fs.Chmod(ctx, "/chmod_link", 0o600)
		if err != nil {
			t.Fatalf("Chmod through symlink failed: %v", err)
		}

		// Verify target's mode changed
		stats, err := fs.Stat(ctx, "/chmod_target.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if stats.Mode&0o777 != 0o600 {
			t.Errorf("Mode = %o, want 0600", stats.Mode&0o777)
		}
	})

	t.Run("deep symlink chain at limit", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		// Create a chain of exactly MaxSymlinkDepth symlinks
		fs.WriteFile(ctx, "/final_target.txt", []byte("deep"), 0o644)

		prev := "/final_target.txt"
		for i := 0; i < MaxSymlinkDepth; i++ {
			name := "/chain_" + itoa(i)
			fs.Symlink(ctx, prev, name)
			prev = name
		}

		// The last link should still resolve (exactly MaxSymlinkDepth hops)
		stats, err := fs.Stat(ctx, prev)
		if err != nil {
			t.Fatalf("Stat at MaxSymlinkDepth chain should succeed: %v", err)
		}
		if !stats.IsRegularFile() {
			t.Error("Expected regular file")
		}

		// One more symlink should cause ELOOP
		fs.Symlink(ctx, prev, "/one_too_many")
		_, err = fs.Stat(ctx, "/one_too_many")
		if err == nil {
			t.Fatal("Expected ELOOP for chain exceeding MaxSymlinkDepth")
		}
		if !IsLoop(err) {
			t.Errorf("Expected ELOOP, got: %v", err)
		}
	})

	t.Run("relative symlink as intermediate directory", func(t *testing.T) {
		fs, ctx := setupSymlinkTest(t)
		fs.MkdirAll(ctx, "/a/b", 0o755)
		fs.WriteFile(ctx, "/a/b/file.txt", []byte("nested"), 0o644)
		// /a/link -> b (relative, resolves to /a/b)
		fs.Symlink(ctx, "b", "/a/link")

		data, err := fs.ReadFile(ctx, "/a/link/file.txt")
		if err != nil {
			t.Fatalf("ReadFile through relative intermediate symlink failed: %v", err)
		}
		if string(data) != "nested" {
			t.Errorf("Content = %q, want %q", string(data), "nested")
		}
	})
}

// itoa is a simple int to string converter for test use.
func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	s := ""
	for n > 0 {
		s = string(rune('0'+n%10)) + s
		n /= 10
	}
	return s
}
