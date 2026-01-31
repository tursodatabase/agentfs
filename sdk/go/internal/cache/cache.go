// Package cache provides caching implementations for AgentFS.
package cache

// PathCache caches path-to-inode resolution results.
type PathCache interface {
	// Get returns the inode for a path, or (0, false) if not cached.
	Get(path string) (ino int64, ok bool)

	// Set caches a path-to-inode mapping.
	Set(path string, ino int64)

	// Delete removes a single path from the cache.
	Delete(path string)

	// DeletePrefix removes all paths with the given prefix.
	// Used when a directory is deleted or renamed.
	DeletePrefix(prefix string)

	// Clear removes all entries.
	Clear()

	// Stats returns cache statistics.
	Stats() Stats
}

// Stats contains cache performance statistics.
type Stats struct {
	Hits       int64 // Number of cache hits
	Misses     int64 // Number of cache misses
	Entries    int   // Current number of entries
	MaxEntries int   // Maximum capacity
}

// HitRate returns the cache hit rate as a percentage (0-100).
// Returns 0 if no lookups have been performed.
func (s Stats) HitRate() float64 {
	total := s.Hits + s.Misses
	if total == 0 {
		return 0
	}
	return float64(s.Hits) / float64(total) * 100
}
