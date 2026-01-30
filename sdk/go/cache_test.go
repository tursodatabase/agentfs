package agentfs

import (
	"context"
	"fmt"
	"testing"
)

func setupTestDBWithCache(t *testing.T) *AgentFS {
	t.Helper()
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		Cache: CacheOptions{
			Enabled:    true,
			MaxEntries: 1000,
		},
	})
	if err != nil {
		t.Fatalf("Failed to open AgentFS with cache: %v", err)
	}
	return afs
}

func TestFilesystemCache_BasicCaching(t *testing.T) {
	afs := setupTestDBWithCache(t)
	defer afs.Close()
	ctx := context.Background()

	// Create a file
	err := afs.FS.WriteFile(ctx, "/test.txt", []byte("hello"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	// First Stat should miss cache
	_, err = afs.FS.Stat(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("Stat failed: %v", err)
	}

	stats := afs.FS.CacheStats()
	if stats == nil {
		t.Fatal("CacheStats should not be nil when cache is enabled")
	}
	initialMisses := stats.Misses

	// Second Stat should hit cache
	_, err = afs.FS.Stat(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("Stat failed: %v", err)
	}

	stats = afs.FS.CacheStats()
	if stats.Hits == 0 {
		t.Error("Expected cache hit on second Stat")
	}
	if stats.Misses != initialMisses {
		t.Error("Expected no additional misses on second Stat")
	}
}

func TestFilesystemCache_InvalidationOnUnlink(t *testing.T) {
	afs := setupTestDBWithCache(t)
	defer afs.Close()
	ctx := context.Background()

	// Create and stat a file to populate cache
	afs.FS.WriteFile(ctx, "/to_delete.txt", []byte("x"), 0o644)
	afs.FS.Stat(ctx, "/to_delete.txt")

	// Verify it's in cache
	stats := afs.FS.CacheStats()
	entriesBefore := stats.Entries

	// Delete the file
	err := afs.FS.Unlink(ctx, "/to_delete.txt")
	if err != nil {
		t.Fatalf("Unlink failed: %v", err)
	}

	// Cache entry should be invalidated
	stats = afs.FS.CacheStats()
	if stats.Entries >= entriesBefore {
		t.Error("Cache entry should be invalidated after Unlink")
	}

	// Stat should fail (file gone)
	_, err = afs.FS.Stat(ctx, "/to_delete.txt")
	if !IsNotExist(err) {
		t.Error("Expected ENOENT after Unlink")
	}
}

func TestFilesystemCache_InvalidationOnRmdir(t *testing.T) {
	afs := setupTestDBWithCache(t)
	defer afs.Close()
	ctx := context.Background()

	// Create directory structure
	afs.FS.MkdirAll(ctx, "/dir/subdir", 0o755)
	afs.FS.WriteFile(ctx, "/dir/subdir/file.txt", []byte("x"), 0o644)

	// Populate cache
	afs.FS.Stat(ctx, "/dir")
	afs.FS.Stat(ctx, "/dir/subdir")
	afs.FS.Stat(ctx, "/dir/subdir/file.txt")

	// Remove file and subdirectory
	afs.FS.Unlink(ctx, "/dir/subdir/file.txt")
	afs.FS.Rmdir(ctx, "/dir/subdir")

	// /dir/subdir and children should be invalidated
	_, err := afs.FS.Stat(ctx, "/dir/subdir")
	if !IsNotExist(err) {
		t.Error("Expected ENOENT for removed directory")
	}

	// /dir should still exist
	_, err = afs.FS.Stat(ctx, "/dir")
	if err != nil {
		t.Errorf("Parent directory should still exist: %v", err)
	}
}

func TestFilesystemCache_InvalidationOnRename(t *testing.T) {
	afs := setupTestDBWithCache(t)
	defer afs.Close()
	ctx := context.Background()

	// Create file and populate cache
	afs.FS.WriteFile(ctx, "/old_name.txt", []byte("x"), 0o644)
	afs.FS.Stat(ctx, "/old_name.txt")

	// Rename
	err := afs.FS.Rename(ctx, "/old_name.txt", "/new_name.txt")
	if err != nil {
		t.Fatalf("Rename failed: %v", err)
	}

	// Old path should be invalidated
	_, err = afs.FS.Stat(ctx, "/old_name.txt")
	if !IsNotExist(err) {
		t.Error("Expected ENOENT for old path after rename")
	}

	// New path should work
	_, err = afs.FS.Stat(ctx, "/new_name.txt")
	if err != nil {
		t.Errorf("New path should exist: %v", err)
	}
}

func TestFilesystemCache_InvalidationOnDirectoryRename(t *testing.T) {
	afs := setupTestDBWithCache(t)
	defer afs.Close()
	ctx := context.Background()

	// Create directory with contents
	afs.FS.MkdirAll(ctx, "/olddir/sub", 0o755)
	afs.FS.WriteFile(ctx, "/olddir/file.txt", []byte("x"), 0o644)
	afs.FS.WriteFile(ctx, "/olddir/sub/nested.txt", []byte("y"), 0o644)

	// Populate cache
	afs.FS.Stat(ctx, "/olddir")
	afs.FS.Stat(ctx, "/olddir/file.txt")
	afs.FS.Stat(ctx, "/olddir/sub")
	afs.FS.Stat(ctx, "/olddir/sub/nested.txt")

	// Rename directory
	err := afs.FS.Rename(ctx, "/olddir", "/newdir")
	if err != nil {
		t.Fatalf("Rename directory failed: %v", err)
	}

	// Old paths should not exist
	_, err = afs.FS.Stat(ctx, "/olddir")
	if !IsNotExist(err) {
		t.Error("Expected ENOENT for old directory")
	}
	_, err = afs.FS.Stat(ctx, "/olddir/file.txt")
	if !IsNotExist(err) {
		t.Error("Expected ENOENT for old child file")
	}

	// New paths should exist
	_, err = afs.FS.Stat(ctx, "/newdir")
	if err != nil {
		t.Error("New directory should exist")
	}
	_, err = afs.FS.Stat(ctx, "/newdir/file.txt")
	if err != nil {
		t.Error("New child file should exist")
	}
}

func TestFilesystemCache_ClearCache(t *testing.T) {
	afs := setupTestDBWithCache(t)
	defer afs.Close()
	ctx := context.Background()

	// Create files and populate cache
	for i := 0; i < 10; i++ {
		path := fmt.Sprintf("/file%d.txt", i)
		afs.FS.WriteFile(ctx, path, []byte("x"), 0o644)
		afs.FS.Stat(ctx, path)
	}

	stats := afs.FS.CacheStats()
	if stats.Entries == 0 {
		t.Error("Cache should have entries")
	}

	// Clear cache
	afs.FS.ClearCache()

	stats = afs.FS.CacheStats()
	if stats.Entries != 0 {
		t.Error("Cache should be empty after ClearCache")
	}
}

func TestFilesystemCache_Disabled(t *testing.T) {
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		// Cache not enabled
	})
	if err != nil {
		t.Fatalf("Failed to open AgentFS: %v", err)
	}
	defer afs.Close()

	// CacheStats should return nil when cache is disabled
	stats := afs.FS.CacheStats()
	if stats != nil {
		t.Error("CacheStats should be nil when cache is disabled")
	}

	// Operations should still work
	err = afs.FS.WriteFile(ctx, "/test.txt", []byte("hello"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	_, err = afs.FS.Stat(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("Stat failed: %v", err)
	}
}

// Benchmarks comparing cached vs uncached performance

func BenchmarkPathResolution_NoCache(b *testing.B) {
	depths := []int{1, 5, 10, 20}

	for _, depth := range depths {
		b.Run(fmt.Sprintf("depth_%d", depth), func(b *testing.B) {
			ctx := context.Background()
			afs, _ := Open(ctx, AgentFSOptions{Path: ":memory:"})
			defer afs.Close()

			// Create deep directory structure
			path := ""
			for d := 0; d < depth; d++ {
				path += fmt.Sprintf("/dir%d", d)
			}
			afs.FS.MkdirAll(ctx, path, 0o755)
			filePath := path + "/file.txt"
			afs.FS.WriteFile(ctx, filePath, []byte("x"), 0o644)

			b.ResetTimer()
			for i := 0; i < b.N; i++ {
				afs.FS.Stat(ctx, filePath)
			}
		})
	}
}

func BenchmarkPathResolution_WithCache(b *testing.B) {
	depths := []int{1, 5, 10, 20}

	for _, depth := range depths {
		b.Run(fmt.Sprintf("depth_%d", depth), func(b *testing.B) {
			ctx := context.Background()
			afs, _ := Open(ctx, AgentFSOptions{
				Path: ":memory:",
				Cache: CacheOptions{
					Enabled:    true,
					MaxEntries: 10000,
				},
			})
			defer afs.Close()

			// Create deep directory structure
			path := ""
			for d := 0; d < depth; d++ {
				path += fmt.Sprintf("/dir%d", d)
			}
			afs.FS.MkdirAll(ctx, path, 0o755)
			filePath := path + "/file.txt"
			afs.FS.WriteFile(ctx, filePath, []byte("x"), 0o644)

			// Warm the cache
			afs.FS.Stat(ctx, filePath)

			b.ResetTimer()
			for i := 0; i < b.N; i++ {
				afs.FS.Stat(ctx, filePath)
			}
		})
	}
}

func BenchmarkCacheHitRate(b *testing.B) {
	ctx := context.Background()
	afs, _ := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		Cache: CacheOptions{
			Enabled:    true,
			MaxEntries: 10000,
		},
	})
	defer afs.Close()

	// Create 100 files
	for i := 0; i < 100; i++ {
		path := fmt.Sprintf("/file%d.txt", i)
		afs.FS.WriteFile(ctx, path, []byte("x"), 0o644)
	}

	// Warm cache by reading all files
	for i := 0; i < 100; i++ {
		path := fmt.Sprintf("/file%d.txt", i)
		afs.FS.Stat(ctx, path)
	}

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		// Access random-ish files (but all should be cached)
		path := fmt.Sprintf("/file%d.txt", i%100)
		afs.FS.Stat(ctx, path)
	}

	b.StopTimer()
	stats := afs.FS.CacheStats()
	b.ReportMetric(stats.HitRate(), "hit_rate_%")
}

// =============================================================================
// Parameterized Cache Consistency Tests for Filesystem
// =============================================================================
// These tests run the same operations with caching enabled and disabled
// to verify that results are consistent regardless of cache state.

type fsCacheTestConfig struct {
	name         string
	cacheEnabled bool
}

var fsCacheConfigs = []fsCacheTestConfig{
	{"WithCache", true},
	{"WithoutCache", false},
}

func setupFSWithConfig(t *testing.T, cfg fsCacheTestConfig) *AgentFS {
	t.Helper()
	ctx := context.Background()

	opts := AgentFSOptions{Path: ":memory:"}
	if cfg.cacheEnabled {
		opts.Cache = CacheOptions{
			Enabled:    true,
			MaxEntries: 1000,
		}
	}

	afs, err := Open(ctx, opts)
	if err != nil {
		t.Fatalf("Failed to open AgentFS: %v", err)
	}
	return afs
}

// TestFSCacheConsistency_FileOperations tests basic file operations
// produce identical results with and without caching.
func TestFSCacheConsistency_FileOperations(t *testing.T) {
	for _, cfg := range fsCacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			afs := setupFSWithConfig(t, cfg)
			defer afs.Close()

			// Test 1: Write and read file
			err := afs.FS.WriteFile(ctx, "/test.txt", []byte("hello world"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Test 2: Read file multiple times (exercises cache)
			for i := 0; i < 5; i++ {
				content, err := afs.FS.ReadFile(ctx, "/test.txt")
				if err != nil {
					t.Fatalf("ReadFile iteration %d failed: %v", i, err)
				}
				if string(content) != "hello world" {
					t.Errorf("Iteration %d: Expected 'hello world', got %q", i, string(content))
				}
			}

			// Test 3: Stat file multiple times
			for i := 0; i < 5; i++ {
				stats, err := afs.FS.Stat(ctx, "/test.txt")
				if err != nil {
					t.Fatalf("Stat iteration %d failed: %v", i, err)
				}
				if stats.Size != 11 {
					t.Errorf("Iteration %d: Expected size 11, got %d", i, stats.Size)
				}
			}

			// Test 4: Overwrite file
			err = afs.FS.WriteFile(ctx, "/test.txt", []byte("overwritten"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile overwrite failed: %v", err)
			}

			// Test 5: Read overwritten content multiple times
			for i := 0; i < 5; i++ {
				content, err := afs.FS.ReadFile(ctx, "/test.txt")
				if err != nil {
					t.Fatalf("ReadFile after overwrite iteration %d failed: %v", i, err)
				}
				if string(content) != "overwritten" {
					t.Errorf("Iteration %d: Expected 'overwritten', got %q", i, string(content))
				}
			}
		})
	}
}

// TestFSCacheConsistency_DirectoryOperations tests directory operations
// produce identical results with and without caching.
func TestFSCacheConsistency_DirectoryOperations(t *testing.T) {
	for _, cfg := range fsCacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			afs := setupFSWithConfig(t, cfg)
			defer afs.Close()

			// Test 1: Create directory
			err := afs.FS.Mkdir(ctx, "/mydir", 0o755)
			if err != nil {
				t.Fatalf("Mkdir failed: %v", err)
			}

			// Test 2: Stat directory multiple times
			for i := 0; i < 3; i++ {
				stats, err := afs.FS.Stat(ctx, "/mydir")
				if err != nil {
					t.Fatalf("Stat dir iteration %d failed: %v", i, err)
				}
				if !stats.IsDir() {
					t.Errorf("Iteration %d: Expected directory", i)
				}
			}

			// Test 3: MkdirAll with deep path
			err = afs.FS.MkdirAll(ctx, "/deep/nested/path", 0o755)
			if err != nil {
				t.Fatalf("MkdirAll failed: %v", err)
			}

			// Test 4: Verify intermediate paths exist
			paths := []string{"/deep", "/deep/nested", "/deep/nested/path"}
			for _, p := range paths {
				for i := 0; i < 3; i++ {
					stats, err := afs.FS.Stat(ctx, p)
					if err != nil {
						t.Fatalf("Stat %s iteration %d failed: %v", p, i, err)
					}
					if !stats.IsDir() {
						t.Errorf("Stat %s iteration %d: Expected directory", p, i)
					}
				}
			}

			// Test 5: Readdir
			err = afs.FS.WriteFile(ctx, "/mydir/file1.txt", []byte("1"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}
			err = afs.FS.WriteFile(ctx, "/mydir/file2.txt", []byte("2"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			entries, err := afs.FS.Readdir(ctx, "/mydir")
			if err != nil {
				t.Fatalf("Readdir failed: %v", err)
			}
			if len(entries) != 2 {
				t.Errorf("Expected 2 entries, got %d", len(entries))
			}
		})
	}
}

// TestFSCacheConsistency_DeleteOperations tests delete operations
// produce identical results with and without caching.
func TestFSCacheConsistency_DeleteOperations(t *testing.T) {
	for _, cfg := range fsCacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			afs := setupFSWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Create files
			err := afs.FS.WriteFile(ctx, "/to_delete.txt", []byte("delete me"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Test 1: Stat file multiple times to populate cache
			for i := 0; i < 3; i++ {
				_, err := afs.FS.Stat(ctx, "/to_delete.txt")
				if err != nil {
					t.Fatalf("Stat before delete iteration %d failed: %v", i, err)
				}
			}

			// Test 2: Delete file
			err = afs.FS.Unlink(ctx, "/to_delete.txt")
			if err != nil {
				t.Fatalf("Unlink failed: %v", err)
			}

			// Test 3: Multiple lookups should all fail
			for i := 0; i < 3; i++ {
				_, err = afs.FS.Stat(ctx, "/to_delete.txt")
				if !IsNotExist(err) {
					t.Errorf("Iteration %d: Expected ENOENT after delete, got %v", i, err)
				}
			}

			// Test 4: Create and delete directory
			err = afs.FS.Mkdir(ctx, "/dir_to_delete", 0o755)
			if err != nil {
				t.Fatalf("Mkdir failed: %v", err)
			}

			// Populate cache
			for i := 0; i < 3; i++ {
				afs.FS.Stat(ctx, "/dir_to_delete")
			}

			err = afs.FS.Rmdir(ctx, "/dir_to_delete")
			if err != nil {
				t.Fatalf("Rmdir failed: %v", err)
			}

			// Test 5: Multiple lookups should all fail
			for i := 0; i < 3; i++ {
				_, err = afs.FS.Stat(ctx, "/dir_to_delete")
				if !IsNotExist(err) {
					t.Errorf("Iteration %d: Expected ENOENT after rmdir, got %v", i, err)
				}
			}
		})
	}
}

// TestFSCacheConsistency_RenameOperations tests rename operations
// produce identical results with and without caching.
func TestFSCacheConsistency_RenameOperations(t *testing.T) {
	for _, cfg := range fsCacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			afs := setupFSWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Create file
			err := afs.FS.WriteFile(ctx, "/old_name.txt", []byte("content"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Test 1: Populate cache with multiple lookups
			for i := 0; i < 3; i++ {
				_, err := afs.FS.Stat(ctx, "/old_name.txt")
				if err != nil {
					t.Fatalf("Stat iteration %d failed: %v", i, err)
				}
			}

			// Test 2: Rename file
			err = afs.FS.Rename(ctx, "/old_name.txt", "/new_name.txt")
			if err != nil {
				t.Fatalf("Rename failed: %v", err)
			}

			// Test 3: Old path should not exist
			for i := 0; i < 3; i++ {
				_, err = afs.FS.Stat(ctx, "/old_name.txt")
				if !IsNotExist(err) {
					t.Errorf("Iteration %d: Expected ENOENT for old path", i)
				}
			}

			// Test 4: New path should exist with correct content
			for i := 0; i < 3; i++ {
				content, err := afs.FS.ReadFile(ctx, "/new_name.txt")
				if err != nil {
					t.Fatalf("ReadFile iteration %d failed: %v", i, err)
				}
				if string(content) != "content" {
					t.Errorf("Iteration %d: Expected 'content', got %q", i, string(content))
				}
			}

			// Test 5: Rename directory with contents
			err = afs.FS.MkdirAll(ctx, "/olddir/sub", 0o755)
			if err != nil {
				t.Fatalf("MkdirAll failed: %v", err)
			}
			err = afs.FS.WriteFile(ctx, "/olddir/file.txt", []byte("in dir"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile in dir failed: %v", err)
			}

			// Populate cache
			afs.FS.Stat(ctx, "/olddir")
			afs.FS.Stat(ctx, "/olddir/sub")
			afs.FS.Stat(ctx, "/olddir/file.txt")

			err = afs.FS.Rename(ctx, "/olddir", "/newdir")
			if err != nil {
				t.Fatalf("Rename dir failed: %v", err)
			}

			// Test 6: Old paths should not exist
			_, err = afs.FS.Stat(ctx, "/olddir")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT for old dir")
			}
			_, err = afs.FS.Stat(ctx, "/olddir/file.txt")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT for old dir child")
			}

			// Test 7: New paths should exist
			_, err = afs.FS.Stat(ctx, "/newdir")
			if err != nil {
				t.Errorf("New dir should exist: %v", err)
			}
			content, err := afs.FS.ReadFile(ctx, "/newdir/file.txt")
			if err != nil {
				t.Errorf("ReadFile from new dir failed: %v", err)
			}
			if string(content) != "in dir" {
				t.Errorf("Expected 'in dir', got %q", string(content))
			}
		})
	}
}

// TestFSCacheConsistency_LinkOperations tests link operations
// produce identical results with and without caching.
func TestFSCacheConsistency_LinkOperations(t *testing.T) {
	for _, cfg := range fsCacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			afs := setupFSWithConfig(t, cfg)
			defer afs.Close()

			// Test 1: Create file and hard link
			err := afs.FS.WriteFile(ctx, "/original.txt", []byte("shared content"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			err = afs.FS.Link(ctx, "/original.txt", "/hardlink.txt")
			if err != nil {
				t.Fatalf("Link failed: %v", err)
			}

			// Test 2: Both paths should have same content
			for i := 0; i < 3; i++ {
				content1, err := afs.FS.ReadFile(ctx, "/original.txt")
				if err != nil {
					t.Fatalf("ReadFile original iteration %d failed: %v", i, err)
				}
				content2, err := afs.FS.ReadFile(ctx, "/hardlink.txt")
				if err != nil {
					t.Fatalf("ReadFile hardlink iteration %d failed: %v", i, err)
				}
				if string(content1) != string(content2) {
					t.Errorf("Iteration %d: Content mismatch", i)
				}
			}

			// Test 3: Create symlink
			err = afs.FS.Symlink(ctx, "/target_path", "/symlink")
			if err != nil {
				t.Fatalf("Symlink failed: %v", err)
			}

			// Test 4: Read symlink multiple times
			for i := 0; i < 3; i++ {
				target, err := afs.FS.Readlink(ctx, "/symlink")
				if err != nil {
					t.Fatalf("Readlink iteration %d failed: %v", i, err)
				}
				if target != "/target_path" {
					t.Errorf("Iteration %d: Expected '/target_path', got %q", i, target)
				}
			}
		})
	}
}

// TestFSCacheConsistency_DeepPathResolution tests deep path resolution
// is consistent with and without caching.
func TestFSCacheConsistency_DeepPathResolution(t *testing.T) {
	for _, cfg := range fsCacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			afs := setupFSWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Create deep directory structure
			err := afs.FS.MkdirAll(ctx, "/a/b/c/d/e", 0o755)
			if err != nil {
				t.Fatalf("MkdirAll failed: %v", err)
			}
			err = afs.FS.WriteFile(ctx, "/a/b/c/d/e/deep_file.txt", []byte("deep content"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Test 1: Read deep file multiple times
			for i := 0; i < 5; i++ {
				content, err := afs.FS.ReadFile(ctx, "/a/b/c/d/e/deep_file.txt")
				if err != nil {
					t.Fatalf("ReadFile iteration %d failed: %v", i, err)
				}
				if string(content) != "deep content" {
					t.Errorf("Iteration %d: Expected 'deep content', got %q", i, string(content))
				}
			}

			// Test 2: Stat all intermediate paths multiple times
			paths := []string{"/a", "/a/b", "/a/b/c", "/a/b/c/d", "/a/b/c/d/e"}
			for _, p := range paths {
				for i := 0; i < 3; i++ {
					stats, err := afs.FS.Stat(ctx, p)
					if err != nil {
						t.Fatalf("Stat %s iteration %d failed: %v", p, i, err)
					}
					if !stats.IsDir() {
						t.Errorf("Expected %s to be directory", p)
					}
				}
			}

			// Test 3: Delete deep file
			err = afs.FS.Unlink(ctx, "/a/b/c/d/e/deep_file.txt")
			if err != nil {
				t.Fatalf("Unlink failed: %v", err)
			}

			// Test 4: Multiple lookups should fail
			for i := 0; i < 3; i++ {
				_, err = afs.FS.Stat(ctx, "/a/b/c/d/e/deep_file.txt")
				if !IsNotExist(err) {
					t.Errorf("Iteration %d: Expected ENOENT after delete", i)
				}
			}

			// Test 5: Parent directories should still exist
			for _, p := range paths {
				stats, err := afs.FS.Stat(ctx, p)
				if err != nil {
					t.Errorf("Parent %s should still exist: %v", p, err)
				}
				if !stats.IsDir() {
					t.Errorf("Expected %s to be directory", p)
				}
			}
		})
	}
}

// TestFSCacheConsistency_ChmodOperations tests chmod operations
// produce identical results with and without caching.
func TestFSCacheConsistency_ChmodOperations(t *testing.T) {
	for _, cfg := range fsCacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			afs := setupFSWithConfig(t, cfg)
			defer afs.Close()

			// Setup
			err := afs.FS.WriteFile(ctx, "/chmod_test.txt", []byte("test"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Test 1: Stat multiple times to populate cache
			for i := 0; i < 3; i++ {
				stats, err := afs.FS.Stat(ctx, "/chmod_test.txt")
				if err != nil {
					t.Fatalf("Stat iteration %d failed: %v", i, err)
				}
				if stats.Mode&0o777 != 0o644 {
					t.Errorf("Iteration %d: Expected mode 0644, got %o", i, stats.Mode&0o777)
				}
			}

			// Test 2: Chmod
			err = afs.FS.Chmod(ctx, "/chmod_test.txt", 0o600)
			if err != nil {
				t.Fatalf("Chmod failed: %v", err)
			}

			// Test 3: Stat should reflect new mode
			for i := 0; i < 3; i++ {
				stats, err := afs.FS.Stat(ctx, "/chmod_test.txt")
				if err != nil {
					t.Fatalf("Stat after chmod iteration %d failed: %v", i, err)
				}
				if stats.Mode&0o777 != 0o600 {
					t.Errorf("Iteration %d: Expected mode 0600, got %o", i, stats.Mode&0o777)
				}
			}
		})
	}
}

// TestFSCacheConsistency_ReaddirOperations tests readdir operations
// produce identical results with and without caching.
func TestFSCacheConsistency_ReaddirOperations(t *testing.T) {
	for _, cfg := range fsCacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			afs := setupFSWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Create directory with files
			err := afs.FS.Mkdir(ctx, "/testdir", 0o755)
			if err != nil {
				t.Fatalf("Mkdir failed: %v", err)
			}
			for i := 0; i < 5; i++ {
				path := fmt.Sprintf("/testdir/file%d.txt", i)
				err = afs.FS.WriteFile(ctx, path, []byte(fmt.Sprintf("content%d", i)), 0o644)
				if err != nil {
					t.Fatalf("WriteFile failed: %v", err)
				}
			}

			// Test 1: Readdir multiple times
			for iter := 0; iter < 3; iter++ {
				entries, err := afs.FS.Readdir(ctx, "/testdir")
				if err != nil {
					t.Fatalf("Readdir iteration %d failed: %v", iter, err)
				}
				if len(entries) != 5 {
					t.Errorf("Iteration %d: Expected 5 entries, got %d", iter, len(entries))
				}
			}

			// Test 2: Delete a file
			err = afs.FS.Unlink(ctx, "/testdir/file2.txt")
			if err != nil {
				t.Fatalf("Unlink failed: %v", err)
			}

			// Test 3: Readdir should reflect deletion
			for iter := 0; iter < 3; iter++ {
				entries, err := afs.FS.Readdir(ctx, "/testdir")
				if err != nil {
					t.Fatalf("Readdir after delete iteration %d failed: %v", iter, err)
				}
				if len(entries) != 4 {
					t.Errorf("Iteration %d: Expected 4 entries, got %d", iter, len(entries))
				}
				for _, e := range entries {
					if e == "file2.txt" {
						t.Errorf("Iteration %d: Deleted file should not appear in readdir", iter)
					}
				}
			}

			// Test 4: Add a new file
			err = afs.FS.WriteFile(ctx, "/testdir/new_file.txt", []byte("new"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile new file failed: %v", err)
			}

			// Test 5: Readdir should include new file
			entries, err := afs.FS.Readdir(ctx, "/testdir")
			if err != nil {
				t.Fatalf("Readdir after add failed: %v", err)
			}
			if len(entries) != 5 {
				t.Errorf("Expected 5 entries after add, got %d", len(entries))
			}
			found := false
			for _, e := range entries {
				if e == "new_file.txt" {
					found = true
					break
				}
			}
			if !found {
				t.Error("New file should appear in readdir")
			}
		})
	}
}
