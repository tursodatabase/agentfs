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
