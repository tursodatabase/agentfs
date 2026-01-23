package cache

import (
	"strings"
	"sync"
	"sync/atomic"
	"time"

	lru "github.com/hashicorp/golang-lru/v2"
)

// lruCache implements PathCache using an LRU eviction policy.
type lruCache struct {
	cache      *lru.Cache[string, cacheEntry]
	ttl        time.Duration
	mu         sync.RWMutex
	hits       atomic.Int64
	misses     atomic.Int64
	maxEntries int
}

// cacheEntry stores the cached inode and optional expiration time.
type cacheEntry struct {
	ino       int64
	expiresAt time.Time // zero if no TTL
}

// NewLRU creates a new LRU-based path cache.
//
// Parameters:
//   - maxEntries: maximum number of entries to cache
//   - ttl: time-to-live for entries (0 = no expiration)
func NewLRU(maxEntries int, ttl time.Duration) (PathCache, error) {
	cache, err := lru.New[string, cacheEntry](maxEntries)
	if err != nil {
		return nil, err
	}
	return &lruCache{
		cache:      cache,
		ttl:        ttl,
		maxEntries: maxEntries,
	}, nil
}

// Get returns the inode for a path, or (0, false) if not cached or expired.
func (c *lruCache) Get(path string) (int64, bool) {
	c.mu.RLock()
	entry, ok := c.cache.Get(path)
	c.mu.RUnlock()

	if !ok {
		c.misses.Add(1)
		return 0, false
	}

	// Check TTL
	if !entry.expiresAt.IsZero() && time.Now().After(entry.expiresAt) {
		c.mu.Lock()
		c.cache.Remove(path)
		c.mu.Unlock()
		c.misses.Add(1)
		return 0, false
	}

	c.hits.Add(1)
	return entry.ino, true
}

// Set caches a path-to-inode mapping.
func (c *lruCache) Set(path string, ino int64) {
	entry := cacheEntry{ino: ino}
	if c.ttl > 0 {
		entry.expiresAt = time.Now().Add(c.ttl)
	}

	c.mu.Lock()
	c.cache.Add(path, entry)
	c.mu.Unlock()
}

// Delete removes a single path from the cache.
func (c *lruCache) Delete(path string) {
	c.mu.Lock()
	c.cache.Remove(path)
	c.mu.Unlock()
}

// DeletePrefix removes all paths with the given prefix.
func (c *lruCache) DeletePrefix(prefix string) {
	c.mu.Lock()
	defer c.mu.Unlock()

	// Get all keys and remove those with matching prefix
	keys := c.cache.Keys()
	for _, key := range keys {
		if strings.HasPrefix(key, prefix) {
			c.cache.Remove(key)
		}
	}
}

// Clear removes all entries from the cache.
func (c *lruCache) Clear() {
	c.mu.Lock()
	c.cache.Purge()
	c.mu.Unlock()
}

// Stats returns cache statistics.
func (c *lruCache) Stats() Stats {
	c.mu.RLock()
	entries := c.cache.Len()
	c.mu.RUnlock()

	return Stats{
		Hits:       c.hits.Load(),
		Misses:     c.misses.Load(),
		Entries:    entries,
		MaxEntries: c.maxEntries,
	}
}
