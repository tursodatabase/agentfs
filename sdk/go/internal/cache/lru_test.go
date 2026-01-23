package cache

import (
	"testing"
	"time"
)

func TestLRUCache_BasicOperations(t *testing.T) {
	cache, err := NewLRU(100, 0)
	if err != nil {
		t.Fatalf("NewLRU failed: %v", err)
	}

	// Test Set and Get
	cache.Set("/foo/bar", 42)

	ino, ok := cache.Get("/foo/bar")
	if !ok {
		t.Error("Expected cache hit for /foo/bar")
	}
	if ino != 42 {
		t.Errorf("Expected ino 42, got %d", ino)
	}

	// Test cache miss
	_, ok = cache.Get("/nonexistent")
	if ok {
		t.Error("Expected cache miss for /nonexistent")
	}
}

func TestLRUCache_Delete(t *testing.T) {
	cache, _ := NewLRU(100, 0)

	cache.Set("/foo", 1)
	cache.Set("/bar", 2)

	cache.Delete("/foo")

	_, ok := cache.Get("/foo")
	if ok {
		t.Error("Expected cache miss after delete")
	}

	ino, ok := cache.Get("/bar")
	if !ok || ino != 2 {
		t.Error("Other entries should not be affected")
	}
}

func TestLRUCache_DeletePrefix(t *testing.T) {
	cache, _ := NewLRU(100, 0)

	cache.Set("/dir", 1)
	cache.Set("/dir/a", 2)
	cache.Set("/dir/b", 3)
	cache.Set("/dir/sub/c", 4)
	cache.Set("/other", 5)

	cache.DeletePrefix("/dir/")

	// /dir itself should still exist
	if _, ok := cache.Get("/dir"); !ok {
		t.Error("/dir should still exist")
	}

	// Children should be deleted
	if _, ok := cache.Get("/dir/a"); ok {
		t.Error("/dir/a should be deleted")
	}
	if _, ok := cache.Get("/dir/b"); ok {
		t.Error("/dir/b should be deleted")
	}
	if _, ok := cache.Get("/dir/sub/c"); ok {
		t.Error("/dir/sub/c should be deleted")
	}

	// Other paths should remain
	if _, ok := cache.Get("/other"); !ok {
		t.Error("/other should still exist")
	}
}

func TestLRUCache_Clear(t *testing.T) {
	cache, _ := NewLRU(100, 0)

	cache.Set("/a", 1)
	cache.Set("/b", 2)
	cache.Set("/c", 3)

	cache.Clear()

	if _, ok := cache.Get("/a"); ok {
		t.Error("Cache should be empty after clear")
	}
	if _, ok := cache.Get("/b"); ok {
		t.Error("Cache should be empty after clear")
	}

	stats := cache.Stats()
	if stats.Entries != 0 {
		t.Errorf("Expected 0 entries, got %d", stats.Entries)
	}
}

func TestLRUCache_Stats(t *testing.T) {
	cache, _ := NewLRU(100, 0)

	// Initial stats
	stats := cache.Stats()
	if stats.Hits != 0 || stats.Misses != 0 {
		t.Error("Initial stats should be zero")
	}
	if stats.MaxEntries != 100 {
		t.Errorf("Expected MaxEntries 100, got %d", stats.MaxEntries)
	}

	// Generate some hits and misses
	cache.Set("/exists", 1)
	cache.Get("/exists")  // hit
	cache.Get("/exists")  // hit
	cache.Get("/missing") // miss

	stats = cache.Stats()
	if stats.Hits != 2 {
		t.Errorf("Expected 2 hits, got %d", stats.Hits)
	}
	if stats.Misses != 1 {
		t.Errorf("Expected 1 miss, got %d", stats.Misses)
	}
	if stats.Entries != 1 {
		t.Errorf("Expected 1 entry, got %d", stats.Entries)
	}
}

func TestLRUCache_HitRate(t *testing.T) {
	stats := Stats{Hits: 75, Misses: 25}
	rate := stats.HitRate()
	if rate != 75.0 {
		t.Errorf("Expected 75.0%% hit rate, got %f", rate)
	}

	// Zero case
	stats = Stats{Hits: 0, Misses: 0}
	rate = stats.HitRate()
	if rate != 0 {
		t.Errorf("Expected 0%% hit rate for empty stats, got %f", rate)
	}
}

func TestLRUCache_Eviction(t *testing.T) {
	cache, _ := NewLRU(3, 0)

	cache.Set("/a", 1)
	cache.Set("/b", 2)
	cache.Set("/c", 3)

	// Access /a to make it recently used
	cache.Get("/a")

	// Add /d, should evict /b (least recently used)
	cache.Set("/d", 4)

	if _, ok := cache.Get("/a"); !ok {
		t.Error("/a should still exist (recently used)")
	}
	if _, ok := cache.Get("/b"); ok {
		t.Error("/b should be evicted (LRU)")
	}
	if _, ok := cache.Get("/c"); !ok {
		t.Error("/c should still exist")
	}
	if _, ok := cache.Get("/d"); !ok {
		t.Error("/d should exist")
	}
}

func TestLRUCache_TTL(t *testing.T) {
	cache, _ := NewLRU(100, 50*time.Millisecond)

	cache.Set("/expires", 1)

	// Should exist immediately
	if _, ok := cache.Get("/expires"); !ok {
		t.Error("Entry should exist immediately")
	}

	// Wait for TTL to expire
	time.Sleep(60 * time.Millisecond)

	// Should be expired now
	if _, ok := cache.Get("/expires"); ok {
		t.Error("Entry should have expired")
	}
}

func TestLRUCache_NoTTL(t *testing.T) {
	cache, _ := NewLRU(100, 0) // 0 = no TTL

	cache.Set("/permanent", 1)

	// Wait a bit
	time.Sleep(10 * time.Millisecond)

	// Should still exist
	if _, ok := cache.Get("/permanent"); !ok {
		t.Error("Entry with no TTL should not expire")
	}
}

func TestLRUCache_Concurrent(t *testing.T) {
	cache, _ := NewLRU(1000, 0)

	// Run concurrent operations
	done := make(chan bool)

	// Writer goroutine
	go func() {
		for i := 0; i < 100; i++ {
			cache.Set("/path", int64(i))
		}
		done <- true
	}()

	// Reader goroutine
	go func() {
		for i := 0; i < 100; i++ {
			cache.Get("/path")
		}
		done <- true
	}()

	// Deleter goroutine
	go func() {
		for i := 0; i < 100; i++ {
			cache.Delete("/other")
		}
		done <- true
	}()

	// Wait for all goroutines
	<-done
	<-done
	<-done

	// Should not panic or deadlock
}
