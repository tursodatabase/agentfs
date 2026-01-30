package agentfs

import (
	"context"
	"database/sql"
	"path"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// Layer indicates which layer an inode belongs to in an overlay filesystem.
type Layer int

const (
	// LayerDelta is the writable upper layer where modifications are stored.
	LayerDelta Layer = iota
	// LayerBase is the read-only lower layer.
	LayerBase
)

// InodeInfo tracks information about an inode in the overlay filesystem.
type InodeInfo struct {
	Layer         Layer
	UnderlyingIno int64
	Path          string
}

// BaseFS is the interface that the base (read-only) filesystem must implement.
// This allows using any filesystem implementation as the base layer.
type BaseFS interface {
	// Stat returns file/directory metadata for the given inode.
	Stat(ctx context.Context, ino int64) (*Stats, error)

	// Lookup finds an entry in a directory by name.
	Lookup(ctx context.Context, parentIno int64, name string) (*Stats, error)

	// Readdir returns the names of entries in a directory.
	Readdir(ctx context.Context, ino int64) ([]string, error)

	// ReaddirPlus returns directory entries with their stats.
	ReaddirPlus(ctx context.Context, ino int64) ([]DirEntry, error)

	// ReadFile reads the entire contents of a file.
	ReadFile(ctx context.Context, ino int64) ([]byte, error)

	// Readlink returns the target of a symbolic link.
	Readlink(ctx context.Context, ino int64) (string, error)
}

// OverlayFS provides a copy-on-write overlay filesystem.
// It combines a read-only base layer with a writable delta layer (AgentFS).
// All modifications are written to the delta layer, while reads fall back
// to the base layer if not found in delta.
type OverlayFS struct {
	base  BaseFS
	delta *Filesystem
	db    *sql.DB

	// Inode mapping: overlay_ino -> InodeInfo
	inodeMap sync.Map // map[int64]InodeInfo

	// Reverse mapping: (layer, underlying_ino) -> overlay_ino
	reverseMap sync.Map // map[layerInoKey]int64

	// Path to overlay inode mapping
	pathMap sync.Map // map[string]int64

	// Whiteout paths (deleted from base)
	whiteouts sync.Map // map[string]bool

	// Origin mapping: delta_ino -> base_ino
	originMap sync.Map // map[int64]int64

	// Next inode number to allocate
	nextIno atomic.Int64
}

// layerInoKey is a key for the reverse map.
type layerInoKey struct {
	layer Layer
	ino   int64
}

// NewOverlayFS creates a new overlay filesystem.
// The base layer is read-only, and the delta layer receives all modifications.
func NewOverlayFS(base BaseFS, delta *Filesystem, db *sql.DB) *OverlayFS {
	ofs := &OverlayFS{
		base:  base,
		delta: delta,
		db:    db,
	}

	// Initialize root inode mapping (root is always inode 1 in both layers)
	ofs.inodeMap.Store(int64(RootIno), InodeInfo{
		Layer:         LayerDelta,
		UnderlyingIno: RootIno,
		Path:          "/",
	})
	ofs.reverseMap.Store(layerInoKey{LayerDelta, RootIno}, int64(RootIno))
	ofs.pathMap.Store("/", int64(RootIno))

	// Start allocating inodes from 2
	ofs.nextIno.Store(2)

	return ofs
}

// Init initializes the overlay filesystem by loading persisted state.
// Call this after creating an OverlayFS for an existing database.
func (ofs *OverlayFS) Init(ctx context.Context) error {
	if err := ofs.loadWhiteouts(ctx); err != nil {
		return err
	}
	if err := ofs.loadOrigins(ctx); err != nil {
		return err
	}
	return nil
}

// Base returns the base filesystem.
func (ofs *OverlayFS) Base() BaseFS {
	return ofs.base
}

// Delta returns the delta filesystem.
func (ofs *OverlayFS) Delta() *Filesystem {
	return ofs.delta
}

// allocIno allocates a new overlay inode number.
func (ofs *OverlayFS) allocIno() int64 {
	return ofs.nextIno.Add(1) - 1
}

// getOrCreateOverlayIno gets or creates an overlay inode for a layer inode.
func (ofs *OverlayFS) getOrCreateOverlayIno(layer Layer, underlyingIno int64, path string) int64 {
	key := layerInoKey{layer, underlyingIno}

	// Check reverse map first
	if ino, ok := ofs.reverseMap.Load(key); ok {
		return ino.(int64)
	}

	// Allocate new inode
	ino := ofs.allocIno()

	ofs.inodeMap.Store(ino, InodeInfo{
		Layer:         layer,
		UnderlyingIno: underlyingIno,
		Path:          path,
	})
	ofs.reverseMap.Store(key, ino)
	ofs.pathMap.Store(path, ino)

	return ino
}

// getOrCreateOverlayInoWithOrigin gets or creates an overlay inode for a file that was copied up.
// Uses the base inode for stable numbering (kernel cache consistency) but maps to delta for data.
// This ensures that after copy-up, Stat returns delta data (with modifications) but the inode
// number remains stable across the copy-up operation.
func (ofs *OverlayFS) getOrCreateOverlayInoWithOrigin(baseIno, deltaIno int64, path string) int64 {
	// Check if we already have an overlay inode for this base inode
	key := layerInoKey{LayerBase, baseIno}
	if ino, ok := ofs.reverseMap.Load(key); ok {
		overlayIno := ino.(int64)
		// Update the mapping to point to delta layer for data access
		ofs.inodeMap.Store(overlayIno, InodeInfo{
			Layer:         LayerDelta,
			UnderlyingIno: deltaIno,
			Path:          path,
		})
		// Also add delta to reverse map
		ofs.reverseMap.Store(layerInoKey{LayerDelta, deltaIno}, overlayIno)
		return overlayIno
	}

	// Allocate new inode
	ino := ofs.allocIno()

	// Map overlay inode to delta layer (for reading actual data after copy-up)
	ofs.inodeMap.Store(ino, InodeInfo{
		Layer:         LayerDelta,
		UnderlyingIno: deltaIno,
		Path:          path,
	})

	// Store reverse mappings for both base and delta
	// Base key ensures stable inode for files that existed in base
	ofs.reverseMap.Store(key, ino)
	ofs.reverseMap.Store(layerInoKey{LayerDelta, deltaIno}, ino)
	ofs.pathMap.Store(path, ino)

	return ino
}

// getInodeInfo returns inode info for an overlay inode.
func (ofs *OverlayFS) getInodeInfo(ino int64) (InodeInfo, bool) {
	if info, ok := ofs.inodeMap.Load(ino); ok {
		return info.(InodeInfo), true
	}
	return InodeInfo{}, false
}

// buildPath builds a path from parent inode and name.
func (ofs *OverlayFS) buildPath(parentIno int64, name string) (string, error) {
	info, ok := ofs.getInodeInfo(parentIno)
	if !ok {
		return "", ErrNoent("buildPath", "")
	}
	if info.Path == "/" {
		return "/" + name, nil
	}
	return info.Path + "/" + name, nil
}

// parentPath returns the parent path of a given path.
func parentPath(p string) string {
	if p == "/" {
		return "/"
	}
	parent := path.Dir(p)
	if parent == "." {
		return "/"
	}
	return parent
}

// =============================================================================
// Whiteout Operations
// =============================================================================

// loadWhiteouts loads whiteouts from the database into memory.
func (ofs *OverlayFS) loadWhiteouts(ctx context.Context) error {
	rows, err := ofs.db.QueryContext(ctx, whiteoutList)
	if err != nil {
		return err
	}
	defer rows.Close()

	for rows.Next() {
		var path string
		if err := rows.Scan(&path); err != nil {
			return err
		}
		ofs.whiteouts.Store(path, true)
	}
	return rows.Err()
}

// createWhiteout creates a whiteout for a path.
func (ofs *OverlayFS) createWhiteout(ctx context.Context, p string) error {
	now := time.Now().Unix()
	parent := parentPath(p)

	_, err := ofs.db.ExecContext(ctx, whiteoutInsert, p, parent, now)
	if err != nil {
		return err
	}
	ofs.whiteouts.Store(p, true)
	return nil
}

// removeWhiteout removes a whiteout for a path.
func (ofs *OverlayFS) removeWhiteout(ctx context.Context, p string) error {
	if _, ok := ofs.whiteouts.Load(p); !ok {
		return nil // Not a whiteout
	}

	_, err := ofs.db.ExecContext(ctx, whiteoutDelete, p)
	if err != nil {
		return err
	}
	ofs.whiteouts.Delete(p)
	return nil
}

// isWhiteout checks if a path or any of its ancestors is whiteout.
func (ofs *OverlayFS) isWhiteout(p string) bool {
	// Check path and all ancestors
	current := ""
	for _, component := range strings.Split(p, "/") {
		if component == "" {
			continue
		}
		current = current + "/" + component
		if _, ok := ofs.whiteouts.Load(current); ok {
			return true
		}
	}
	return false
}

// getChildWhiteouts returns the names of whiteout entries that are direct children of a directory.
func (ofs *OverlayFS) getChildWhiteouts(dirPath string) map[string]bool {
	result := make(map[string]bool)

	prefix := dirPath
	if prefix != "/" {
		prefix = prefix + "/"
	}

	ofs.whiteouts.Range(func(key, value interface{}) bool {
		p := key.(string)
		if dirPath == "/" {
			// Direct children of root
			trimmed := strings.TrimPrefix(p, "/")
			if !strings.Contains(trimmed, "/") && trimmed != "" {
				result[trimmed] = true
			}
		} else if strings.HasPrefix(p, prefix) {
			rest := p[len(prefix):]
			if !strings.Contains(rest, "/") && rest != "" {
				result[rest] = true
			}
		}
		return true
	})

	return result
}

// =============================================================================
// Origin Tracking
// =============================================================================

// loadOrigins loads origin mappings from the database.
func (ofs *OverlayFS) loadOrigins(ctx context.Context) error {
	rows, err := ofs.db.QueryContext(ctx, originList)
	if err != nil {
		// Table might not exist in older databases
		return nil
	}
	defer rows.Close()

	for rows.Next() {
		var deltaIno, baseIno int64
		if err := rows.Scan(&deltaIno, &baseIno); err != nil {
			return err
		}
		ofs.originMap.Store(deltaIno, baseIno)
	}
	return rows.Err()
}

// addOriginMapping stores an origin mapping for copy-up.
func (ofs *OverlayFS) addOriginMapping(ctx context.Context, deltaIno, baseIno int64) error {
	_, err := ofs.db.ExecContext(ctx, originInsert, deltaIno, baseIno)
	if err != nil {
		return err
	}
	ofs.originMap.Store(deltaIno, baseIno)
	return nil
}

// getOriginIno gets the base inode for a delta inode if it was copied up.
func (ofs *OverlayFS) getOriginIno(deltaIno int64) (int64, bool) {
	if baseIno, ok := ofs.originMap.Load(deltaIno); ok {
		return baseIno.(int64), true
	}
	return 0, false
}

// =============================================================================
// Copy-Up Operations
// =============================================================================

// ensureParentDirs ensures parent directories exist in the delta layer.
func (ofs *OverlayFS) ensureParentDirs(ctx context.Context, p string, uid, gid int64) error {
	components := strings.Split(strings.Trim(p, "/"), "/")
	if len(components) <= 1 {
		return nil // No parent directories to create
	}

	currentPath := ""
	currentDeltaIno := int64(RootIno)
	currentBaseIno := int64(RootIno)

	// Process all but the last component (the file/dir being created)
	for _, component := range components[:len(components)-1] {
		currentPath = currentPath + "/" + component

		// Remove any whiteout for this path
		if err := ofs.removeWhiteout(ctx, currentPath); err != nil {
			return err
		}

		// Check if directory exists in delta
		deltaStats, err := ofs.delta.lookupDentry(ctx, currentDeltaIno, component)
		if err == nil {
			// Exists in delta
			stats, err := ofs.delta.statInode(ctx, deltaStats)
			if err != nil {
				return err
			}
			if !stats.IsDir() {
				return ErrNotDir("ensureParentDirs", currentPath)
			}
			currentDeltaIno = deltaStats

			// Advance base in parallel
			if baseStats, err := ofs.base.Lookup(ctx, currentBaseIno, component); err == nil && baseStats != nil {
				currentBaseIno = baseStats.Ino
			}
			continue
		}

		// Not in delta, check base
		var dirUID, dirGID int64
		var originBaseIno int64
		baseStats, err := ofs.base.Lookup(ctx, currentBaseIno, component)
		if err == nil && baseStats != nil {
			dirUID = baseStats.UID
			dirGID = baseStats.GID
			originBaseIno = baseStats.Ino
			currentBaseIno = baseStats.Ino
		} else {
			dirUID = uid
			dirGID = gid
		}

		// Create directory in delta
		now := time.Now()
		nowSec := now.Unix()
		nowNsec := int64(now.Nanosecond())
		dirMode := S_IFDIR | 0o755

		var newIno int64
		err = ofs.db.QueryRowContext(ctx, insertInode, dirMode, dirUID, dirGID, 0, nowSec, nowSec, nowSec, 0, nowNsec, nowNsec, nowNsec).Scan(&newIno)
		if err != nil {
			return err
		}

		if _, err := ofs.db.ExecContext(ctx, insertDentry, component, currentDeltaIno, newIno); err != nil {
			return err
		}
		if _, err := ofs.db.ExecContext(ctx, incrementNlink, newIno); err != nil {
			return err
		}

		// Create origin mapping if directory exists in base
		if originBaseIno != 0 {
			if err := ofs.addOriginMapping(ctx, newIno, originBaseIno); err != nil {
				return err
			}
		}

		currentDeltaIno = newIno
	}

	return nil
}

// copyUp copies a file from base to delta for modification.
// Returns the delta inode number.
func (ofs *OverlayFS) copyUp(ctx context.Context, p string, baseIno int64) (int64, error) {
	components := strings.Split(strings.Trim(p, "/"), "/")
	if len(components) == 0 {
		return 0, ErrPerm("copyUp", "/")
	}
	name := components[len(components)-1]

	// Check if already copied up
	parentIno := int64(RootIno)
	foundParent := true
	for _, comp := range components[:len(components)-1] {
		ino, err := ofs.delta.lookupDentry(ctx, parentIno, comp)
		if err != nil {
			foundParent = false
			break
		}
		parentIno = ino
	}

	if foundParent {
		if ino, err := ofs.delta.lookupDentry(ctx, parentIno, name); err == nil {
			// Already copied up
			return ino, nil
		}
	}

	// Get base stats
	baseStats, err := ofs.base.Stat(ctx, baseIno)
	if err != nil {
		return 0, err
	}

	// Ensure parent directories exist
	if err := ofs.ensureParentDirs(ctx, p, baseStats.UID, baseStats.GID); err != nil {
		return 0, err
	}

	// Look up parent in delta by walking the path
	parentIno = RootIno
	for _, comp := range components[:len(components)-1] {
		ino, err := ofs.delta.lookupDentry(ctx, parentIno, comp)
		if err != nil {
			return 0, ErrNoent("copyUp", p)
		}
		parentIno = ino
	}

	now := time.Now()
	nowSec := now.Unix()
	nowNsec := int64(now.Nanosecond())

	var deltaIno int64

	if baseStats.IsSymlink() {
		// Copy symlink
		target, err := ofs.base.Readlink(ctx, baseIno)
		if err != nil {
			return 0, err
		}

		err = ofs.db.QueryRowContext(ctx, insertInode, S_IFLNK|0o777, baseStats.UID, baseStats.GID, len(target), nowSec, nowSec, nowSec, 0, nowNsec, nowNsec, nowNsec).Scan(&deltaIno)
		if err != nil {
			return 0, err
		}

		if _, err := ofs.db.ExecContext(ctx, insertSymlink, deltaIno, target); err != nil {
			return 0, err
		}
	} else if baseStats.IsDir() {
		// Copy directory (structure only, not contents)
		err = ofs.db.QueryRowContext(ctx, insertInode, baseStats.Mode, baseStats.UID, baseStats.GID, 0, nowSec, nowSec, nowSec, 0, nowNsec, nowNsec, nowNsec).Scan(&deltaIno)
		if err != nil {
			return 0, err
		}
	} else {
		// Copy regular file
		content, err := ofs.base.ReadFile(ctx, baseIno)
		if err != nil {
			return 0, err
		}

		err = ofs.db.QueryRowContext(ctx, insertInode, baseStats.Mode, baseStats.UID, baseStats.GID, len(content), nowSec, nowSec, nowSec, 0, nowNsec, nowNsec, nowNsec).Scan(&deltaIno)
		if err != nil {
			return 0, err
		}

		// Write data chunks
		if err := ofs.delta.writeChunks(ctx, deltaIno, content); err != nil {
			return 0, err
		}
	}

	// Create dentry
	if _, err := ofs.db.ExecContext(ctx, insertDentry, name, parentIno, deltaIno); err != nil {
		return 0, err
	}
	if _, err := ofs.db.ExecContext(ctx, incrementNlink, deltaIno); err != nil {
		return 0, err
	}

	// Store origin mapping
	if err := ofs.addOriginMapping(ctx, deltaIno, baseIno); err != nil {
		return 0, err
	}

	return deltaIno, nil
}

// copyUpAndUpdateMapping copies up a file and updates the inode mapping.
func (ofs *OverlayFS) copyUpAndUpdateMapping(ctx context.Context, overlayIno int64, info InodeInfo) (int64, error) {
	deltaIno, err := ofs.copyUp(ctx, info.Path, info.UnderlyingIno)
	if err != nil {
		return 0, err
	}

	// Update the inode mapping to point to delta
	ofs.inodeMap.Store(overlayIno, InodeInfo{
		Layer:         LayerDelta,
		UnderlyingIno: deltaIno,
		Path:          info.Path,
	})

	// Add delta to reverse map (keep base mapping for origin lookups)
	ofs.reverseMap.Store(layerInoKey{LayerDelta, deltaIno}, overlayIno)

	return deltaIno, nil
}

// =============================================================================
// Overlay Filesystem Operations
// =============================================================================

// Lookup finds an entry in a directory by name.
func (ofs *OverlayFS) Lookup(ctx context.Context, parentIno int64, name string) (*Stats, error) {
	parentInfo, ok := ofs.getInodeInfo(parentIno)
	if !ok {
		return nil, ErrNoent("lookup", "")
	}

	p, err := ofs.buildPath(parentIno, name)
	if err != nil {
		return nil, err
	}

	// Check for whiteout
	if ofs.isWhiteout(p) {
		return nil, ErrNoent("lookup", p)
	}

	// Try delta first
	var deltaParentIno int64
	if parentInfo.Layer == LayerDelta {
		deltaParentIno = parentInfo.UnderlyingIno
	} else {
		// Walk delta to find corresponding parent
		deltaParentIno = RootIno
		for _, comp := range strings.Split(strings.Trim(parentInfo.Path, "/"), "/") {
			if comp == "" {
				continue
			}
			ino, err := ofs.delta.lookupDentry(ctx, deltaParentIno, comp)
			if err != nil {
				deltaParentIno = 0
				break
			}
			deltaParentIno = ino
		}
	}

	if deltaParentIno != 0 {
		if deltaChildIno, err := ofs.delta.lookupDentry(ctx, deltaParentIno, name); err == nil {
			deltaStats, err := ofs.delta.statInode(ctx, deltaChildIno)
			if err != nil {
				return nil, err
			}

			// Check for origin mapping to return stable inode
			// We return delta stats (for correct data) but with stable overlay inode
			if baseIno, ok := ofs.getOriginIno(deltaStats.Ino); ok {
				// Get or create overlay inode for this path
				// Use base inode as key for stable numbering, but map to delta for data
				overlayIno := ofs.getOrCreateOverlayInoWithOrigin(baseIno, deltaChildIno, p)
				deltaStats.Ino = overlayIno
			} else {
				deltaStats.Ino = ofs.getOrCreateOverlayIno(LayerDelta, deltaStats.Ino, p)
			}

			return deltaStats, nil
		}
	}

	// Try base
	var baseParentIno int64
	if parentInfo.Layer == LayerBase {
		baseParentIno = parentInfo.UnderlyingIno
	} else {
		// Walk base to find corresponding parent
		baseParentIno = RootIno
		for _, comp := range strings.Split(strings.Trim(parentInfo.Path, "/"), "/") {
			if comp == "" {
				continue
			}
			stats, err := ofs.base.Lookup(ctx, baseParentIno, comp)
			if err != nil || stats == nil {
				return nil, ErrNoent("lookup", p)
			}
			baseParentIno = stats.Ino
		}
	}

	baseStats, err := ofs.base.Lookup(ctx, baseParentIno, name)
	if err != nil || baseStats == nil {
		return nil, ErrNoent("lookup", p)
	}

	baseStats.Ino = ofs.getOrCreateOverlayIno(LayerBase, baseStats.Ino, p)
	return baseStats, nil
}

// Stat returns file/directory metadata for the given overlay inode.
func (ofs *OverlayFS) Stat(ctx context.Context, ino int64) (*Stats, error) {
	info, ok := ofs.getInodeInfo(ino)
	if !ok {
		return nil, ErrNoent("stat", "")
	}

	var stats *Stats
	var err error

	switch info.Layer {
	case LayerDelta:
		stats, err = ofs.delta.statInode(ctx, info.UnderlyingIno)
	case LayerBase:
		stats, err = ofs.base.Stat(ctx, info.UnderlyingIno)
	}

	if err != nil {
		return nil, err
	}

	stats.Ino = ino
	return stats, nil
}

// Readdir returns the names of entries in a directory.
func (ofs *OverlayFS) Readdir(ctx context.Context, ino int64) ([]string, error) {
	info, ok := ofs.getInodeInfo(ino)
	if !ok {
		return nil, ErrNoent("readdir", "")
	}

	childWhiteouts := ofs.getChildWhiteouts(info.Path)
	entries := make(map[string]bool)

	// Get delta entries
	if info.Layer == LayerDelta {
		deltaEntries, err := ofs.delta.Readdir(ctx, info.Path)
		if err == nil {
			for _, name := range deltaEntries {
				entries[name] = true
			}
		}
	}

	// Get base entries
	var baseIno int64
	if info.Layer == LayerBase {
		baseIno = info.UnderlyingIno
	} else {
		// Walk base to find corresponding directory
		baseIno = RootIno
		foundAll := true
		for _, comp := range strings.Split(strings.Trim(info.Path, "/"), "/") {
			if comp == "" {
				continue
			}
			stats, err := ofs.base.Lookup(ctx, baseIno, comp)
			if err != nil || stats == nil {
				foundAll = false
				break
			}
			baseIno = stats.Ino
		}
		if !foundAll {
			baseIno = 0
		}
	}

	if baseIno != 0 {
		baseEntries, err := ofs.base.Readdir(ctx, baseIno)
		if err == nil {
			for _, name := range baseEntries {
				entryPath := info.Path
				if entryPath == "/" {
					entryPath = "/" + name
				} else {
					entryPath = entryPath + "/" + name
				}
				if !ofs.isWhiteout(entryPath) && !childWhiteouts[name] {
					entries[name] = true
				}
			}
		}
	}

	result := make([]string, 0, len(entries))
	for name := range entries {
		result = append(result, name)
	}
	return result, nil
}

// ReaddirPlus returns directory entries with their stats.
func (ofs *OverlayFS) ReaddirPlus(ctx context.Context, ino int64) ([]DirEntry, error) {
	info, ok := ofs.getInodeInfo(ino)
	if !ok {
		return nil, ErrNoent("readdirplus", "")
	}

	childWhiteouts := ofs.getChildWhiteouts(info.Path)
	entriesMap := make(map[string]DirEntry)

	// Get base entries first (so delta can override)
	var baseIno int64
	if info.Layer == LayerBase {
		baseIno = info.UnderlyingIno
	} else {
		baseIno = RootIno
		foundAll := true
		for _, comp := range strings.Split(strings.Trim(info.Path, "/"), "/") {
			if comp == "" {
				continue
			}
			stats, err := ofs.base.Lookup(ctx, baseIno, comp)
			if err != nil || stats == nil {
				foundAll = false
				break
			}
			baseIno = stats.Ino
		}
		if !foundAll {
			baseIno = 0
		}
	}

	if baseIno != 0 {
		baseEntries, err := ofs.base.ReaddirPlus(ctx, baseIno)
		if err == nil {
			for _, entry := range baseEntries {
				entryPath := info.Path
				if entryPath == "/" {
					entryPath = "/" + entry.Name
				} else {
					entryPath = entryPath + "/" + entry.Name
				}

				if !ofs.isWhiteout(entryPath) && !childWhiteouts[entry.Name] {
					overlayIno := ofs.getOrCreateOverlayIno(LayerBase, entry.Stats.Ino, entryPath)
					entry.Stats.Ino = overlayIno
					entriesMap[entry.Name] = entry
				}
			}
		}
	}

	// Get delta entries (override base)
	if info.Layer == LayerDelta {
		deltaEntries, err := ofs.delta.ReaddirPlus(ctx, info.Path)
		if err == nil {
			for _, entry := range deltaEntries {
				entryPath := info.Path
				if entryPath == "/" {
					entryPath = "/" + entry.Name
				} else {
					entryPath = entryPath + "/" + entry.Name
				}

				// Check for origin mapping
				if baseIno, ok := ofs.getOriginIno(entry.Stats.Ino); ok {
					entry.Stats.Ino = ofs.getOrCreateOverlayIno(LayerBase, baseIno, entryPath)
				} else {
					entry.Stats.Ino = ofs.getOrCreateOverlayIno(LayerDelta, entry.Stats.Ino, entryPath)
				}

				entriesMap[entry.Name] = entry
			}
		}
	}

	result := make([]DirEntry, 0, len(entriesMap))
	for _, entry := range entriesMap {
		result = append(result, entry)
	}
	return result, nil
}

// WriteFile writes data to a file, creating it if it doesn't exist.
// Creates in the delta layer and removes any whiteout.
func (ofs *OverlayFS) WriteFile(ctx context.Context, p string, data []byte, mode int64) error {
	p = normalizePath(p)

	// Remove whiteout if exists
	if err := ofs.removeWhiteout(ctx, p); err != nil {
		return err
	}

	// Ensure parent directories exist
	parentP := parentPath(p)
	if parentP != "/" {
		// Check if parent exists in overlay
		parentStats, err := ofs.LookupPath(ctx, parentP)
		if err != nil {
			// Parent doesn't exist, create it
			if err := ofs.MkdirAll(ctx, parentP, 0o755); err != nil {
				return err
			}
		} else if !parentStats.IsDir() {
			return ErrNotDir("write", parentP)
		}
	}

	// Write to delta layer
	return ofs.delta.WriteFile(ctx, p, data, mode)
}

// ReadFile reads the entire contents of a file.
func (ofs *OverlayFS) ReadFile(ctx context.Context, p string) ([]byte, error) {
	p = normalizePath(p)

	// Check for whiteout
	if ofs.isWhiteout(p) {
		return nil, ErrNoent("read", p)
	}

	// Try delta first
	data, err := ofs.delta.ReadFile(ctx, p)
	if err == nil {
		return data, nil
	}
	if !IsNotExist(err) {
		return nil, err
	}

	// Try base - need to resolve path to inode
	ino, err := ofs.resolvePathToBaseIno(ctx, p)
	if err != nil {
		return nil, err
	}

	return ofs.base.ReadFile(ctx, ino)
}

// resolvePathToBaseIno resolves a path to a base layer inode.
func (ofs *OverlayFS) resolvePathToBaseIno(ctx context.Context, p string) (int64, error) {
	components := strings.Split(strings.Trim(p, "/"), "/")
	ino := int64(RootIno)

	for _, comp := range components {
		if comp == "" {
			continue
		}
		stats, err := ofs.base.Lookup(ctx, ino, comp)
		if err != nil || stats == nil {
			return 0, ErrNoent("resolve", p)
		}
		ino = stats.Ino
	}

	return ino, nil
}

// LookupPath looks up a path and returns its stats.
func (ofs *OverlayFS) LookupPath(ctx context.Context, p string) (*Stats, error) {
	p = normalizePath(p)

	if p == "/" {
		return ofs.Stat(ctx, RootIno)
	}

	// Check for whiteout
	if ofs.isWhiteout(p) {
		return nil, ErrNoent("lookup", p)
	}

	components := strings.Split(strings.Trim(p, "/"), "/")
	currentIno := int64(RootIno)

	for _, comp := range components {
		stats, err := ofs.Lookup(ctx, currentIno, comp)
		if err != nil {
			return nil, err
		}
		currentIno = stats.Ino
	}

	return ofs.Stat(ctx, currentIno)
}

// Mkdir creates a directory in the delta layer.
func (ofs *OverlayFS) Mkdir(ctx context.Context, p string, mode int64) error {
	p = normalizePath(p)

	// Check if whiteout exists - if so, we're recreating a deleted item
	wasWhiteout := ofs.isWhiteout(p)

	// Remove whiteout if exists
	if err := ofs.removeWhiteout(ctx, p); err != nil {
		return err
	}

	// Check if already exists in delta layer specifically
	// (If it was a whiteout, we know base has it but we want to create new in delta)
	if !wasWhiteout {
		// Only check overlay if it wasn't a whiteout
		if _, err := ofs.LookupPath(ctx, p); err == nil {
			return ErrExist("mkdir", p)
		}
	} else {
		// It was a whiteout, check only delta layer
		_, err := ofs.delta.Stat(ctx, p)
		if err == nil {
			return ErrExist("mkdir", p)
		}
	}

	return ofs.delta.Mkdir(ctx, p, mode)
}

// MkdirAll creates a directory and all parent directories.
func (ofs *OverlayFS) MkdirAll(ctx context.Context, p string, mode int64) error {
	p = normalizePath(p)
	if p == "/" {
		return nil
	}

	components := strings.Split(strings.Trim(p, "/"), "/")
	currentPath := ""

	for _, component := range components {
		currentPath = currentPath + "/" + component

		// Remove whiteout if exists
		if err := ofs.removeWhiteout(ctx, currentPath); err != nil {
			return err
		}

		stats, err := ofs.LookupPath(ctx, currentPath)
		if err != nil {
			if !IsNotExist(err) {
				return err
			}
			// Create in delta
			if err := ofs.delta.Mkdir(ctx, currentPath, mode); err != nil {
				return err
			}
		} else if !stats.IsDir() {
			return ErrNotDir("mkdir", currentPath)
		}
	}

	return nil
}

// Unlink removes a file.
func (ofs *OverlayFS) Unlink(ctx context.Context, p string) error {
	p = normalizePath(p)
	if p == "/" {
		return ErrPerm("unlink", p)
	}

	// Check if exists
	stats, err := ofs.LookupPath(ctx, p)
	if err != nil {
		return err
	}
	if stats.IsDir() {
		return ErrIsDir("unlink", p)
	}

	// Try to remove from delta
	deltaErr := ofs.delta.Unlink(ctx, p)
	if deltaErr != nil && !IsNotExist(deltaErr) {
		return deltaErr
	}

	// Check if exists in base
	baseIno, err := ofs.resolvePathToBaseIno(ctx, p)
	if err == nil && baseIno != 0 {
		// Create whiteout
		if err := ofs.createWhiteout(ctx, p); err != nil {
			return err
		}
	}

	return nil
}

// Rmdir removes an empty directory.
func (ofs *OverlayFS) Rmdir(ctx context.Context, p string) error {
	p = normalizePath(p)
	if p == "/" {
		return ErrPerm("rmdir", p)
	}

	// Check if exists and is a directory
	stats, err := ofs.LookupPath(ctx, p)
	if err != nil {
		return err
	}
	if !stats.IsDir() {
		return ErrNotDir("rmdir", p)
	}

	// Check if directory is empty (in overlay view)
	entries, err := ofs.Readdir(ctx, stats.Ino)
	if err != nil {
		return err
	}
	if len(entries) > 0 {
		return ErrNotEmpty("rmdir", p)
	}

	// Try to remove from delta
	deltaErr := ofs.delta.Rmdir(ctx, p)
	if deltaErr != nil && !IsNotExist(deltaErr) {
		return deltaErr
	}

	// Check if exists in base
	baseIno, err := ofs.resolvePathToBaseIno(ctx, p)
	if err == nil && baseIno != 0 {
		// Create whiteout
		if err := ofs.createWhiteout(ctx, p); err != nil {
			return err
		}
	}

	return nil
}

// Rename moves or renames a file or directory.
func (ofs *OverlayFS) Rename(ctx context.Context, oldPath, newPath string) error {
	oldPath = normalizePath(oldPath)
	newPath = normalizePath(newPath)

	if oldPath == "/" || newPath == "/" {
		return ErrPerm("rename", oldPath)
	}

	// Get old stats (to check if it exists and what type)
	oldStats, err := ofs.LookupPath(ctx, oldPath)
	if err != nil {
		return err
	}

	// Remove whiteout at destination if exists
	if err := ofs.removeWhiteout(ctx, newPath); err != nil {
		return err
	}

	// Ensure the file is in delta (copy-up if needed)
	info, ok := ofs.getInodeInfo(oldStats.Ino)
	if !ok {
		return ErrNoent("rename", oldPath)
	}

	if info.Layer == LayerBase {
		// Need to copy up first
		_, err := ofs.copyUp(ctx, oldPath, info.UnderlyingIno)
		if err != nil {
			return err
		}
	}

	// Perform rename in delta
	if err := ofs.delta.Rename(ctx, oldPath, newPath); err != nil {
		return err
	}

	// Create whiteout at old path if it existed in base
	baseIno, err := ofs.resolvePathToBaseIno(ctx, oldPath)
	if err == nil && baseIno != 0 {
		if err := ofs.createWhiteout(ctx, oldPath); err != nil {
			return err
		}
	}

	return nil
}

// Link creates a hard link.
func (ofs *OverlayFS) Link(ctx context.Context, existingPath, newPath string) error {
	existingPath = normalizePath(existingPath)
	newPath = normalizePath(newPath)

	// Get existing stats
	stats, err := ofs.LookupPath(ctx, existingPath)
	if err != nil {
		return err
	}
	if stats.IsDir() {
		return ErrPerm("link", existingPath)
	}

	// Ensure the file is in delta
	info, ok := ofs.getInodeInfo(stats.Ino)
	if !ok {
		return ErrNoent("link", existingPath)
	}

	if info.Layer == LayerBase {
		// Copy up first
		_, err := ofs.copyUp(ctx, existingPath, info.UnderlyingIno)
		if err != nil {
			return err
		}
	}

	// Remove whiteout at destination if exists
	if err := ofs.removeWhiteout(ctx, newPath); err != nil {
		return err
	}

	return ofs.delta.Link(ctx, existingPath, newPath)
}

// Symlink creates a symbolic link.
func (ofs *OverlayFS) Symlink(ctx context.Context, target, linkPath string) error {
	linkPath = normalizePath(linkPath)

	// Remove whiteout if exists
	if err := ofs.removeWhiteout(ctx, linkPath); err != nil {
		return err
	}

	return ofs.delta.Symlink(ctx, target, linkPath)
}

// Readlink returns the target of a symbolic link.
func (ofs *OverlayFS) Readlink(ctx context.Context, p string) (string, error) {
	p = normalizePath(p)

	// Check for whiteout
	if ofs.isWhiteout(p) {
		return "", ErrNoent("readlink", p)
	}

	// Try delta first
	target, err := ofs.delta.Readlink(ctx, p)
	if err == nil {
		return target, nil
	}
	if !IsNotExist(err) {
		return "", err
	}

	// Try base
	ino, err := ofs.resolvePathToBaseIno(ctx, p)
	if err != nil {
		return "", err
	}

	return ofs.base.Readlink(ctx, ino)
}

// Chmod changes file permissions.
func (ofs *OverlayFS) Chmod(ctx context.Context, p string, mode int64) error {
	p = normalizePath(p)

	stats, err := ofs.LookupPath(ctx, p)
	if err != nil {
		return err
	}

	info, ok := ofs.getInodeInfo(stats.Ino)
	if !ok {
		return ErrNoent("chmod", p)
	}

	if info.Layer == LayerBase {
		// Copy up first
		_, err := ofs.copyUp(ctx, p, info.UnderlyingIno)
		if err != nil {
			return err
		}
	}

	return ofs.delta.Chmod(ctx, p, mode)
}

// Utimes updates file timestamps.
func (ofs *OverlayFS) Utimes(ctx context.Context, p string, atime, mtime int64) error {
	p = normalizePath(p)

	stats, err := ofs.LookupPath(ctx, p)
	if err != nil {
		return err
	}

	info, ok := ofs.getInodeInfo(stats.Ino)
	if !ok {
		return ErrNoent("utimes", p)
	}

	if info.Layer == LayerBase {
		// Copy up first
		_, err := ofs.copyUp(ctx, p, info.UnderlyingIno)
		if err != nil {
			return err
		}
	}

	return ofs.delta.Utimes(ctx, p, atime, mtime)
}
