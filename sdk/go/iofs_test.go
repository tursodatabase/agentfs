package agentfs

import (
	"context"
	"io"
	"io/fs"
	"testing"
	"testing/fstest"
)

func TestIOFS_FSInterface(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	// Create test files
	afs.FS.WriteFile(ctx, "/hello.txt", []byte("Hello, World!"), 0o644)
	afs.FS.WriteFile(ctx, "/subdir/nested.txt", []byte("Nested content"), 0o644)
	afs.FS.MkdirAll(ctx, "/empty", 0o755)

	iofs := NewIOFS(afs.FS)

	t.Run("Open file", func(t *testing.T) {
		f, err := iofs.Open("hello.txt")
		if err != nil {
			t.Fatalf("Open failed: %v", err)
		}
		defer f.Close()

		data, err := io.ReadAll(f)
		if err != nil {
			t.Fatalf("ReadAll failed: %v", err)
		}
		if string(data) != "Hello, World!" {
			t.Errorf("Content = %q, want %q", data, "Hello, World!")
		}
	})

	t.Run("Open directory", func(t *testing.T) {
		f, err := iofs.Open(".")
		if err != nil {
			t.Fatalf("Open failed: %v", err)
		}
		defer f.Close()

		info, err := f.Stat()
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !info.IsDir() {
			t.Error("Root should be a directory")
		}
	})

	t.Run("Open non-existent", func(t *testing.T) {
		_, err := iofs.Open("nonexistent.txt")
		if err == nil {
			t.Error("Expected error for non-existent file")
		}
		if !isPathError(err, fs.ErrNotExist) {
			t.Errorf("Expected ErrNotExist, got %v", err)
		}
	})

	t.Run("Open invalid path", func(t *testing.T) {
		_, err := iofs.Open("/absolute/path")
		if err == nil {
			t.Error("Expected error for invalid path")
		}
		if !isPathError(err, fs.ErrInvalid) {
			t.Errorf("Expected ErrInvalid, got %v", err)
		}
	})

	t.Run("Open nested file", func(t *testing.T) {
		f, err := iofs.Open("subdir/nested.txt")
		if err != nil {
			t.Fatalf("Open failed: %v", err)
		}
		defer f.Close()

		data, err := io.ReadAll(f)
		if err != nil {
			t.Fatalf("ReadAll failed: %v", err)
		}
		if string(data) != "Nested content" {
			t.Errorf("Content = %q, want %q", data, "Nested content")
		}
	})
}

func TestIOFS_StatFS(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/test.txt", []byte("test content"), 0o644)
	afs.FS.MkdirAll(ctx, "/mydir", 0o755)

	iofs := NewIOFS(afs.FS)

	t.Run("Stat file", func(t *testing.T) {
		info, err := iofs.Stat("test.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if info.Name() != "test.txt" {
			t.Errorf("Name = %q, want %q", info.Name(), "test.txt")
		}
		if info.Size() != 12 {
			t.Errorf("Size = %d, want 12", info.Size())
		}
		if info.IsDir() {
			t.Error("Should not be a directory")
		}
	})

	t.Run("Stat directory", func(t *testing.T) {
		info, err := iofs.Stat("mydir")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !info.IsDir() {
			t.Error("Should be a directory")
		}
		if info.Mode()&fs.ModeDir == 0 {
			t.Error("Mode should have ModeDir set")
		}
	})

	t.Run("Stat root", func(t *testing.T) {
		info, err := iofs.Stat(".")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !info.IsDir() {
			t.Error("Root should be a directory")
		}
	})

	t.Run("Stat non-existent", func(t *testing.T) {
		_, err := iofs.Stat("nonexistent")
		if err == nil {
			t.Error("Expected error")
		}
	})
}

func TestIOFS_ReadFileFS(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	content := []byte("File content for ReadFile test")
	afs.FS.WriteFile(ctx, "/readable.txt", content, 0o644)

	iofs := NewIOFS(afs.FS)

	t.Run("ReadFile", func(t *testing.T) {
		data, err := iofs.ReadFile("readable.txt")
		if err != nil {
			t.Fatalf("ReadFile failed: %v", err)
		}
		if string(data) != string(content) {
			t.Errorf("Content = %q, want %q", data, content)
		}
	})

	t.Run("ReadFile non-existent", func(t *testing.T) {
		_, err := iofs.ReadFile("nonexistent.txt")
		if err == nil {
			t.Error("Expected error")
		}
	})
}

func TestIOFS_ReadDirFS(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/a.txt", []byte("a"), 0o644)
	afs.FS.WriteFile(ctx, "/b.txt", []byte("b"), 0o644)
	afs.FS.MkdirAll(ctx, "/subdir", 0o755)
	afs.FS.WriteFile(ctx, "/subdir/c.txt", []byte("c"), 0o644)

	iofs := NewIOFS(afs.FS)

	t.Run("ReadDir root", func(t *testing.T) {
		entries, err := iofs.ReadDir(".")
		if err != nil {
			t.Fatalf("ReadDir failed: %v", err)
		}

		// Should have a.txt, b.txt, subdir
		if len(entries) != 3 {
			t.Errorf("Got %d entries, want 3", len(entries))
		}

		// Verify sorted order
		names := make([]string, len(entries))
		for i, e := range entries {
			names[i] = e.Name()
		}
		if names[0] != "a.txt" || names[1] != "b.txt" || names[2] != "subdir" {
			t.Errorf("Entries not sorted: %v", names)
		}

		// Check IsDir
		for _, e := range entries {
			if e.Name() == "subdir" {
				if !e.IsDir() {
					t.Error("subdir should be a directory")
				}
			} else {
				if e.IsDir() {
					t.Errorf("%s should not be a directory", e.Name())
				}
			}
		}
	})

	t.Run("ReadDir subdirectory", func(t *testing.T) {
		entries, err := iofs.ReadDir("subdir")
		if err != nil {
			t.Fatalf("ReadDir failed: %v", err)
		}

		if len(entries) != 1 {
			t.Errorf("Got %d entries, want 1", len(entries))
		}
		if entries[0].Name() != "c.txt" {
			t.Errorf("Entry name = %q, want %q", entries[0].Name(), "c.txt")
		}
	})

	t.Run("DirEntry Info", func(t *testing.T) {
		entries, _ := iofs.ReadDir(".")
		for _, e := range entries {
			info, err := e.Info()
			if err != nil {
				t.Errorf("Info() failed for %s: %v", e.Name(), err)
			}
			if info.Name() != e.Name() {
				t.Errorf("Info().Name() = %q, want %q", info.Name(), e.Name())
			}
		}
	})
}

func TestIOFS_ReadDirFile(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/1.txt", []byte("1"), 0o644)
	afs.FS.WriteFile(ctx, "/2.txt", []byte("2"), 0o644)
	afs.FS.WriteFile(ctx, "/3.txt", []byte("3"), 0o644)

	iofs := NewIOFS(afs.FS)

	t.Run("ReadDir incremental", func(t *testing.T) {
		f, err := iofs.Open(".")
		if err != nil {
			t.Fatalf("Open failed: %v", err)
		}
		defer f.Close()

		dir, ok := f.(fs.ReadDirFile)
		if !ok {
			t.Fatal("Directory file should implement ReadDirFile")
		}

		// Read 2 entries
		entries, err := dir.ReadDir(2)
		if err != nil {
			t.Fatalf("ReadDir(2) failed: %v", err)
		}
		if len(entries) != 2 {
			t.Errorf("Got %d entries, want 2", len(entries))
		}

		// Read 2 more (should get 1 remaining)
		entries, err = dir.ReadDir(2)
		if err != nil {
			t.Fatalf("ReadDir(2) failed: %v", err)
		}
		if len(entries) != 1 {
			t.Errorf("Got %d entries, want 1", len(entries))
		}

		// Read more (should get EOF)
		_, err = dir.ReadDir(1)
		if err != io.EOF {
			t.Errorf("Expected EOF, got %v", err)
		}
	})

	t.Run("ReadDir all at once", func(t *testing.T) {
		f, _ := iofs.Open(".")
		defer f.Close()

		dir := f.(fs.ReadDirFile)
		entries, err := dir.ReadDir(-1)
		if err != nil {
			t.Fatalf("ReadDir(-1) failed: %v", err)
		}
		if len(entries) != 3 {
			t.Errorf("Got %d entries, want 3", len(entries))
		}
	})
}

func TestIOFS_Glob(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/a.txt", []byte("a"), 0o644)
	afs.FS.WriteFile(ctx, "/b.txt", []byte("b"), 0o644)
	afs.FS.WriteFile(ctx, "/c.md", []byte("c"), 0o644)
	afs.FS.WriteFile(ctx, "/subdir/d.txt", []byte("d"), 0o644)

	iofs := NewIOFS(afs.FS)

	t.Run("Glob *.txt", func(t *testing.T) {
		// Use fs.Glob from standard library (works with our ReadDir implementation)
		matches, err := fs.Glob(iofs, "*.txt")
		if err != nil {
			t.Fatalf("Glob failed: %v", err)
		}
		if len(matches) != 2 {
			t.Errorf("Got %d matches, want 2: %v", len(matches), matches)
		}
	})

	t.Run("Glob */*.txt", func(t *testing.T) {
		matches, err := fs.Glob(iofs, "*/*.txt")
		if err != nil {
			t.Fatalf("Glob failed: %v", err)
		}
		if len(matches) != 1 {
			t.Errorf("Got %d matches, want 1: %v", len(matches), matches)
		}
	})

	t.Run("Glob no matches", func(t *testing.T) {
		matches, err := fs.Glob(iofs, "*.xyz")
		if err != nil {
			t.Fatalf("Glob failed: %v", err)
		}
		if len(matches) != 0 {
			t.Errorf("Got %d matches, want 0", len(matches))
		}
	})
}

func TestIOFS_SubFS(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/root.txt", []byte("root"), 0o644)
	afs.FS.WriteFile(ctx, "/sub/file.txt", []byte("sub file"), 0o644)
	afs.FS.WriteFile(ctx, "/sub/deep/nested.txt", []byte("nested"), 0o644)

	iofs := NewIOFS(afs.FS)

	t.Run("Sub directory", func(t *testing.T) {
		subFS, err := iofs.Sub("sub")
		if err != nil {
			t.Fatalf("Sub failed: %v", err)
		}

		// file.txt should be accessible as "file.txt" not "sub/file.txt"
		data, err := fs.ReadFile(subFS, "file.txt")
		if err != nil {
			t.Fatalf("ReadFile failed: %v", err)
		}
		if string(data) != "sub file" {
			t.Errorf("Content = %q, want %q", data, "sub file")
		}

		// root.txt should not be accessible
		_, err = fs.ReadFile(subFS, "../root.txt")
		if err == nil {
			t.Error("Expected error accessing parent directory")
		}
	})

	t.Run("Sub nested", func(t *testing.T) {
		subFS, _ := iofs.Sub("sub")
		deepFS, err := subFS.(fs.SubFS).Sub("deep")
		if err != nil {
			t.Fatalf("Sub failed: %v", err)
		}

		data, err := fs.ReadFile(deepFS, "nested.txt")
		if err != nil {
			t.Fatalf("ReadFile failed: %v", err)
		}
		if string(data) != "nested" {
			t.Errorf("Content = %q, want %q", data, "nested")
		}
	})

	t.Run("Sub non-existent", func(t *testing.T) {
		_, err := iofs.Sub("nonexistent")
		if err == nil {
			t.Error("Expected error")
		}
	})

	t.Run("Sub file (not directory)", func(t *testing.T) {
		_, err := iofs.Sub("root.txt")
		if err == nil {
			t.Error("Expected error for Sub on file")
		}
	})

	t.Run("Sub dot returns same", func(t *testing.T) {
		subFS, err := iofs.Sub(".")
		if err != nil {
			t.Fatalf("Sub(.) failed: %v", err)
		}
		if subFS != iofs {
			t.Error("Sub(.) should return same FS")
		}
	})
}

func TestIOFS_WithContext(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/test.txt", []byte("test"), 0o644)

	iofs := NewIOFS(afs.FS)

	t.Run("WithContext creates copy", func(t *testing.T) {
		newCtx := context.WithValue(ctx, "key", "value")
		iofsWithCtx := iofs.WithContext(newCtx)

		if iofsWithCtx == iofs {
			t.Error("WithContext should return a new instance")
		}

		// Should still work
		data, err := iofsWithCtx.ReadFile("test.txt")
		if err != nil {
			t.Fatalf("ReadFile failed: %v", err)
		}
		if string(data) != "test" {
			t.Errorf("Content = %q, want %q", data, "test")
		}
	})
}

func TestIOFS_WalkDir(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/a.txt", []byte("a"), 0o644)
	afs.FS.WriteFile(ctx, "/dir/b.txt", []byte("b"), 0o644)
	afs.FS.WriteFile(ctx, "/dir/subdir/c.txt", []byte("c"), 0o644)

	iofs := NewIOFS(afs.FS)

	t.Run("WalkDir visits all", func(t *testing.T) {
		var visited []string
		err := fs.WalkDir(iofs, ".", func(path string, d fs.DirEntry, err error) error {
			if err != nil {
				return err
			}
			visited = append(visited, path)
			return nil
		})
		if err != nil {
			t.Fatalf("WalkDir failed: %v", err)
		}

		// Should visit: ., a.txt, dir, dir/b.txt, dir/subdir, dir/subdir/c.txt
		if len(visited) != 6 {
			t.Errorf("Visited %d paths, want 6: %v", len(visited), visited)
		}
	})
}

func TestIOFS_FileMode(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/regular.txt", []byte("x"), 0o644)
	afs.FS.MkdirAll(ctx, "/directory", 0o755)
	afs.FS.Symlink(ctx, "/regular.txt", "/symlink")

	iofs := NewIOFS(afs.FS)

	t.Run("Regular file mode", func(t *testing.T) {
		info, _ := iofs.Stat("regular.txt")
		mode := info.Mode()

		if mode.IsDir() {
			t.Error("Regular file should not be a directory")
		}
		if mode&fs.ModeSymlink != 0 {
			t.Error("Regular file should not be a symlink")
		}
		if mode.Perm() != 0o644 {
			t.Errorf("Permissions = %o, want %o", mode.Perm(), 0o644)
		}
	})

	t.Run("Directory mode", func(t *testing.T) {
		info, _ := iofs.Stat("directory")
		mode := info.Mode()

		if !mode.IsDir() {
			t.Error("Directory should be a directory")
		}
		if mode&fs.ModeDir == 0 {
			t.Error("Mode should have ModeDir bit set")
		}
	})

	t.Run("Symlink mode", func(t *testing.T) {
		info, _ := iofs.Stat("symlink")
		mode := info.Mode()

		if mode&fs.ModeSymlink == 0 {
			t.Error("Symlink should have ModeSymlink bit set")
		}
	})
}

func TestIOFS_FSTest(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	// Create test files for fstest.TestFS
	afs.FS.WriteFile(ctx, "/hello.txt", []byte("hello"), 0o644)
	afs.FS.WriteFile(ctx, "/world.txt", []byte("world"), 0o644)
	afs.FS.WriteFile(ctx, "/sub/nested.txt", []byte("nested"), 0o644)

	iofs := NewIOFS(afs.FS)

	// Use the standard library's fstest to validate our implementation
	err := fstest.TestFS(iofs, "hello.txt", "world.txt", "sub/nested.txt")
	if err != nil {
		t.Fatalf("fstest.TestFS failed: %v", err)
	}
}

// Helper function to check for specific path errors
func isPathError(err error, target error) bool {
	if pathErr, ok := err.(*fs.PathError); ok {
		return pathErr.Err == target
	}
	return false
}
