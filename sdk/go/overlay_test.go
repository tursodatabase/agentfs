package agentfs

import (
	"context"
	"testing"
)

// MockBaseFS is a mock implementation of BaseFS for testing.
type MockBaseFS struct {
	files map[int64]*mockFile
	dirs  map[int64]*mockDir
}

type mockFile struct {
	stats   Stats
	content []byte
	target  string // for symlinks
}

type mockDir struct {
	stats   Stats
	entries map[string]int64 // name -> ino
}

func newMockBaseFS() *MockBaseFS {
	fs := &MockBaseFS{
		files: make(map[int64]*mockFile),
		dirs:  make(map[int64]*mockDir),
	}

	// Create root directory
	fs.dirs[1] = &mockDir{
		stats: Stats{
			Ino:   1,
			Mode:  S_IFDIR | 0o755,
			Nlink: 2,
		},
		entries: make(map[string]int64),
	}

	return fs
}

func (fs *MockBaseFS) addFile(parentIno int64, name string, ino int64, content []byte, mode int64) {
	fs.files[ino] = &mockFile{
		stats: Stats{
			Ino:   ino,
			Mode:  S_IFREG | mode,
			Nlink: 1,
			Size:  int64(len(content)),
		},
		content: content,
	}
	if dir, ok := fs.dirs[parentIno]; ok {
		dir.entries[name] = ino
	}
}

func (fs *MockBaseFS) addDir(parentIno int64, name string, ino int64, mode int64) {
	fs.dirs[ino] = &mockDir{
		stats: Stats{
			Ino:   ino,
			Mode:  S_IFDIR | mode,
			Nlink: 2,
		},
		entries: make(map[string]int64),
	}
	if dir, ok := fs.dirs[parentIno]; ok {
		dir.entries[name] = ino
	}
}

func (fs *MockBaseFS) addSymlink(parentIno int64, name string, ino int64, target string) {
	fs.files[ino] = &mockFile{
		stats: Stats{
			Ino:   ino,
			Mode:  S_IFLNK | 0o777,
			Nlink: 1,
			Size:  int64(len(target)),
		},
		target: target,
	}
	if dir, ok := fs.dirs[parentIno]; ok {
		dir.entries[name] = ino
	}
}

func (fs *MockBaseFS) Stat(ctx context.Context, ino int64) (*Stats, error) {
	if f, ok := fs.files[ino]; ok {
		stats := f.stats
		return &stats, nil
	}
	if d, ok := fs.dirs[ino]; ok {
		stats := d.stats
		return &stats, nil
	}
	return nil, ErrNoent("stat", "")
}

func (fs *MockBaseFS) Lookup(ctx context.Context, parentIno int64, name string) (*Stats, error) {
	dir, ok := fs.dirs[parentIno]
	if !ok {
		return nil, ErrNoent("lookup", "")
	}

	childIno, ok := dir.entries[name]
	if !ok {
		return nil, ErrNoent("lookup", name)
	}

	return fs.Stat(ctx, childIno)
}

func (fs *MockBaseFS) Readdir(ctx context.Context, ino int64) ([]string, error) {
	dir, ok := fs.dirs[ino]
	if !ok {
		return nil, ErrNoent("readdir", "")
	}

	names := make([]string, 0, len(dir.entries))
	for name := range dir.entries {
		names = append(names, name)
	}
	return names, nil
}

func (fs *MockBaseFS) ReaddirPlus(ctx context.Context, ino int64) ([]DirEntry, error) {
	dir, ok := fs.dirs[ino]
	if !ok {
		return nil, ErrNoent("readdirplus", "")
	}

	entries := make([]DirEntry, 0, len(dir.entries))
	for name, childIno := range dir.entries {
		stats, err := fs.Stat(ctx, childIno)
		if err != nil {
			continue
		}
		entries = append(entries, DirEntry{Name: name, Stats: stats})
	}
	return entries, nil
}

func (fs *MockBaseFS) ReadFile(ctx context.Context, ino int64) ([]byte, error) {
	f, ok := fs.files[ino]
	if !ok {
		return nil, ErrNoent("read", "")
	}
	if f.stats.IsSymlink() {
		return nil, ErrInval("read", "", "is a symlink")
	}
	return f.content, nil
}

func (fs *MockBaseFS) Readlink(ctx context.Context, ino int64) (string, error) {
	f, ok := fs.files[ino]
	if !ok {
		return "", ErrNoent("readlink", "")
	}
	if !f.stats.IsSymlink() {
		return "", ErrInval("readlink", "", "not a symlink")
	}
	return f.target, nil
}

// setupOverlayTest creates a test overlay filesystem
func setupOverlayTest(t *testing.T) (*OverlayFS, *MockBaseFS, *AgentFS) {
	ctx := context.Background()
	afs := setupTestDB(t)

	base := newMockBaseFS()

	ofs := NewOverlayFS(base, afs.FS, afs.db)
	if err := ofs.Init(ctx); err != nil {
		t.Fatalf("OverlayFS.Init failed: %v", err)
	}

	return ofs, base, afs
}

// =============================================================================
// SPEC Consistency Rule Tests
// =============================================================================

// TestOverlay_ConsistencyRule1_WhiteoutRemovedOnCreate tests:
// "A whiteout MUST be removed when a new file is created at that path"
func TestOverlay_ConsistencyRule1_WhiteoutRemovedOnCreate(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add a file to base
	base.addFile(1, "test.txt", 100, []byte("base content"), 0o644)

	// Verify file is visible
	_, err := ofs.LookupPath(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("LookupPath failed: %v", err)
	}

	// Delete the file (creates whiteout)
	err = ofs.Unlink(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("Unlink failed: %v", err)
	}

	// Verify whiteout exists
	if !ofs.isWhiteout("/test.txt") {
		t.Error("Expected whiteout to exist after Unlink")
	}

	// File should not be visible
	_, err = ofs.LookupPath(ctx, "/test.txt")
	if !IsNotExist(err) {
		t.Errorf("Expected ENOENT after whiteout, got: %v", err)
	}

	// Create a new file at the same path
	err = ofs.WriteFile(ctx, "/test.txt", []byte("new content"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	// Verify whiteout is removed
	if ofs.isWhiteout("/test.txt") {
		t.Error("Expected whiteout to be removed after WriteFile")
	}

	// File should be visible with new content
	data, err := ofs.ReadFile(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("ReadFile failed: %v", err)
	}
	if string(data) != "new content" {
		t.Errorf("Expected 'new content', got %q", data)
	}
}

// TestOverlay_ConsistencyRule2_WhiteoutCreatedOnDelete tests:
// "A whiteout MUST be created when deleting a file that exists in the base layer"
func TestOverlay_ConsistencyRule2_WhiteoutCreatedOnDelete(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add files to base
	base.addFile(1, "base_file.txt", 100, []byte("base content"), 0o644)

	// Add file only in delta
	err := ofs.WriteFile(ctx, "/delta_file.txt", []byte("delta content"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	// Delete base file - should create whiteout
	err = ofs.Unlink(ctx, "/base_file.txt")
	if err != nil {
		t.Fatalf("Unlink base file failed: %v", err)
	}

	if !ofs.isWhiteout("/base_file.txt") {
		t.Error("Expected whiteout for base file after delete")
	}

	// Delete delta file - should NOT create whiteout
	err = ofs.Unlink(ctx, "/delta_file.txt")
	if err != nil {
		t.Fatalf("Unlink delta file failed: %v", err)
	}

	if ofs.isWhiteout("/delta_file.txt") {
		t.Error("Should NOT create whiteout for delta-only file")
	}
}

// TestOverlay_ConsistencyRule3_ParentPathCorrectlyDerived tests:
// "The parent_path MUST be correctly derived from path"
func TestOverlay_ConsistencyRule3_ParentPathCorrectlyDerived(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add nested structure to base
	base.addDir(1, "dir", 100, 0o755)
	base.addFile(100, "file.txt", 101, []byte("content"), 0o644)

	// Delete the nested file
	err := ofs.Unlink(ctx, "/dir/file.txt")
	if err != nil {
		t.Fatalf("Unlink failed: %v", err)
	}

	// Verify whiteout was created with correct parent_path
	// The parent_path should be "/dir" for path "/dir/file.txt"
	var storedParent string
	err = afs.db.QueryRowContext(ctx, "SELECT parent_path FROM fs_whiteout WHERE path = ?", "/dir/file.txt").Scan(&storedParent)
	if err != nil {
		t.Fatalf("Query whiteout failed: %v", err)
	}

	if storedParent != "/dir" {
		t.Errorf("Expected parent_path '/dir', got %q", storedParent)
	}

	// Test root-level file
	base.addFile(1, "root_file.txt", 102, []byte("root"), 0o644)
	err = ofs.Unlink(ctx, "/root_file.txt")
	if err != nil {
		t.Fatalf("Unlink root file failed: %v", err)
	}

	err = afs.db.QueryRowContext(ctx, "SELECT parent_path FROM fs_whiteout WHERE path = ?", "/root_file.txt").Scan(&storedParent)
	if err != nil {
		t.Fatalf("Query whiteout failed: %v", err)
	}

	if storedParent != "/" {
		t.Errorf("Expected parent_path '/', got %q", storedParent)
	}
}

// TestOverlay_ConsistencyRule4_WhiteoutsAffectOverlayLookups tests:
// "Whiteouts only affect overlay lookups, not the underlying base filesystem"
func TestOverlay_ConsistencyRule4_WhiteoutsAffectOverlayLookups(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add file to base
	base.addFile(1, "test.txt", 100, []byte("base content"), 0o644)

	// Verify file exists in base directly
	_, err := base.Lookup(ctx, 1, "test.txt")
	if err != nil {
		t.Fatalf("Base lookup should succeed: %v", err)
	}

	// Delete through overlay (creates whiteout)
	err = ofs.Unlink(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("Unlink failed: %v", err)
	}

	// Overlay lookup should fail
	_, err = ofs.LookupPath(ctx, "/test.txt")
	if !IsNotExist(err) {
		t.Errorf("Overlay lookup should return ENOENT, got: %v", err)
	}

	// Base lookup should still succeed (whiteout doesn't affect base)
	baseStats, err := base.Lookup(ctx, 1, "test.txt")
	if err != nil {
		t.Errorf("Base lookup should still succeed after overlay delete: %v", err)
	}
	if baseStats == nil {
		t.Error("Base lookup returned nil stats")
	}
}

// TestOverlay_ConsistencyRule5_OriginMappingStored tests:
// "When copying a file from base to delta, the origin mapping MUST be stored"
func TestOverlay_ConsistencyRule5_OriginMappingStored(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add file to base
	baseIno := int64(100)
	base.addFile(1, "test.txt", baseIno, []byte("base content"), 0o644)

	// Modify the file through overlay (triggers copy-up)
	err := ofs.Chmod(ctx, "/test.txt", 0o600)
	if err != nil {
		t.Fatalf("Chmod failed: %v", err)
	}

	// Verify origin mapping was stored in database
	var storedBaseIno int64
	rows, err := afs.db.QueryContext(ctx, "SELECT delta_ino, base_ino FROM fs_origin")
	if err != nil {
		t.Fatalf("Query origin failed: %v", err)
	}
	defer rows.Close()

	found := false
	var deltaInoWithOrigin int64
	for rows.Next() {
		var deltaIno int64
		if err := rows.Scan(&deltaIno, &storedBaseIno); err != nil {
			t.Fatalf("Scan failed: %v", err)
		}
		if storedBaseIno == baseIno {
			found = true
			deltaInoWithOrigin = deltaIno
			break
		}
	}

	if !found {
		t.Errorf("Expected origin mapping for base_ino %d to be stored", baseIno)
	}

	// Verify origin mapping is in memory
	originBaseIno, ok := ofs.getOriginIno(deltaInoWithOrigin)
	if !ok {
		t.Error("Origin mapping not found in memory")
	}
	if originBaseIno != baseIno {
		t.Errorf("Origin mapping in memory: expected base_ino %d, got %d", baseIno, originBaseIno)
	}

	// Verify the file now exists in delta layer
	deltaStats, err := ofs.delta.Stat(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("Delta stat failed: %v", err)
	}
	if deltaStats == nil {
		t.Fatal("File should exist in delta after copy-up")
	}
}

// TestOverlay_ConsistencyRule6_BaseInodeReturnedForCopiedUp tests:
// "When stat'ing a delta file with an origin mapping, the base inode MUST be returned"
func TestOverlay_ConsistencyRule6_BaseInodeReturnedForCopiedUp(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add file to base
	baseIno := int64(100)
	base.addFile(1, "test.txt", baseIno, []byte("base content"), 0o644)

	// Get initial overlay inode
	statsBefore, err := ofs.LookupPath(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("LookupPath before copy-up failed: %v", err)
	}
	inoBefore := statsBefore.Ino

	// Modify the file (triggers copy-up)
	err = ofs.Chmod(ctx, "/test.txt", 0o600)
	if err != nil {
		t.Fatalf("Chmod failed: %v", err)
	}

	// Get overlay inode after copy-up
	statsAfter, err := ofs.LookupPath(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("LookupPath after copy-up failed: %v", err)
	}
	inoAfter := statsAfter.Ino

	// The overlay inode should remain stable (same before and after copy-up)
	// This ensures kernel inode caches don't get confused
	if inoBefore != inoAfter {
		t.Errorf("Overlay inode changed after copy-up: %d -> %d", inoBefore, inoAfter)
	}

	// Verify the file was actually modified
	if statsAfter.Mode&0o777 != 0o600 {
		t.Errorf("Expected mode 0600, got %o", statsAfter.Mode&0o777)
	}
}

// =============================================================================
// Additional Overlay Tests
// =============================================================================

func TestOverlay_BasicReadWrite(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add file to base
	base.addFile(1, "base.txt", 100, []byte("base content"), 0o644)

	// Read from base through overlay
	data, err := ofs.ReadFile(ctx, "/base.txt")
	if err != nil {
		t.Fatalf("ReadFile base failed: %v", err)
	}
	if string(data) != "base content" {
		t.Errorf("Expected 'base content', got %q", data)
	}

	// Write to delta
	err = ofs.WriteFile(ctx, "/delta.txt", []byte("delta content"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile delta failed: %v", err)
	}

	// Read from delta
	data, err = ofs.ReadFile(ctx, "/delta.txt")
	if err != nil {
		t.Fatalf("ReadFile delta failed: %v", err)
	}
	if string(data) != "delta content" {
		t.Errorf("Expected 'delta content', got %q", data)
	}
}

func TestOverlay_DeltaOverridesBase(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add file to base
	base.addFile(1, "test.txt", 100, []byte("base content"), 0o644)

	// Overwrite in delta
	err := ofs.WriteFile(ctx, "/test.txt", []byte("delta content"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	// Read should return delta content
	data, err := ofs.ReadFile(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("ReadFile failed: %v", err)
	}
	if string(data) != "delta content" {
		t.Errorf("Expected 'delta content', got %q", data)
	}
}

func TestOverlay_ReaddirMergesLayers(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add files to base
	base.addFile(1, "base1.txt", 100, []byte("base1"), 0o644)
	base.addFile(1, "base2.txt", 101, []byte("base2"), 0o644)

	// Add files to delta
	err := ofs.WriteFile(ctx, "/delta1.txt", []byte("delta1"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	// Readdir should merge both layers
	entries, err := ofs.Readdir(ctx, 1)
	if err != nil {
		t.Fatalf("Readdir failed: %v", err)
	}

	expected := map[string]bool{
		"base1.txt":  true,
		"base2.txt":  true,
		"delta1.txt": true,
	}

	for _, name := range entries {
		if !expected[name] {
			t.Errorf("Unexpected entry: %s", name)
		}
		delete(expected, name)
	}

	for name := range expected {
		t.Errorf("Missing entry: %s", name)
	}
}

func TestOverlay_ReaddirExcludesWhiteouts(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add files to base
	base.addFile(1, "keep.txt", 100, []byte("keep"), 0o644)
	base.addFile(1, "delete.txt", 101, []byte("delete"), 0o644)

	// Delete one file
	err := ofs.Unlink(ctx, "/delete.txt")
	if err != nil {
		t.Fatalf("Unlink failed: %v", err)
	}

	// Readdir should exclude the deleted file
	entries, err := ofs.Readdir(ctx, 1)
	if err != nil {
		t.Fatalf("Readdir failed: %v", err)
	}

	for _, name := range entries {
		if name == "delete.txt" {
			t.Error("Deleted file should not appear in readdir")
		}
	}
}

func TestOverlay_RmdirCreatesWhiteout(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add empty directory to base
	base.addDir(1, "emptydir", 100, 0o755)

	// Remove directory
	err := ofs.Rmdir(ctx, "/emptydir")
	if err != nil {
		t.Fatalf("Rmdir failed: %v", err)
	}

	// Verify whiteout exists
	if !ofs.isWhiteout("/emptydir") {
		t.Error("Expected whiteout after Rmdir")
	}

	// Directory should not be visible
	_, err = ofs.LookupPath(ctx, "/emptydir")
	if !IsNotExist(err) {
		t.Errorf("Expected ENOENT, got: %v", err)
	}
}

func TestOverlay_MkdirRemovesWhiteout(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add directory to base
	base.addDir(1, "testdir", 100, 0o755)

	// Delete it
	err := ofs.Rmdir(ctx, "/testdir")
	if err != nil {
		t.Fatalf("Rmdir failed: %v", err)
	}

	// Recreate it
	err = ofs.Mkdir(ctx, "/testdir", 0o755)
	if err != nil {
		t.Fatalf("Mkdir failed: %v", err)
	}

	// Whiteout should be removed
	if ofs.isWhiteout("/testdir") {
		t.Error("Whiteout should be removed after Mkdir")
	}

	// Directory should be visible
	stats, err := ofs.LookupPath(ctx, "/testdir")
	if err != nil {
		t.Fatalf("LookupPath failed: %v", err)
	}
	if !stats.IsDir() {
		t.Error("Expected directory")
	}
}

func TestOverlay_CopyUpOnModify(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add file to base
	base.addFile(1, "test.txt", 100, []byte("original"), 0o644)

	// Modify through chmod (triggers copy-up)
	err := ofs.Chmod(ctx, "/test.txt", 0o600)
	if err != nil {
		t.Fatalf("Chmod failed: %v", err)
	}

	// Verify the file is now in delta
	stats, err := ofs.delta.Stat(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("Delta stat failed: %v", err)
	}
	if stats.Mode&0o777 != 0o600 {
		t.Errorf("Expected mode 0600, got %o", stats.Mode&0o777)
	}

	// Original base file should be unchanged
	baseStats, err := base.Lookup(ctx, 1, "test.txt")
	if err != nil {
		t.Fatalf("Base lookup failed: %v", err)
	}
	if baseStats.Mode&0o777 != 0o644 {
		t.Errorf("Base file mode should be unchanged: %o", baseStats.Mode&0o777)
	}
}

func TestOverlay_SymlinkCopyUp(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add symlink to base
	base.addSymlink(1, "link", 100, "/target")

	// Read symlink through overlay
	target, err := ofs.Readlink(ctx, "/link")
	if err != nil {
		t.Fatalf("Readlink failed: %v", err)
	}
	if target != "/target" {
		t.Errorf("Expected '/target', got %q", target)
	}
}

func TestOverlay_WhiteoutAncestorBlocks(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t)
	defer afs.Close()

	// Add nested structure to base
	base.addDir(1, "parent", 100, 0o755)
	base.addFile(100, "child.txt", 101, []byte("child"), 0o644)

	// Delete parent directory (after ensuring it's "empty" in overlay view)
	// First, delete the child
	err := ofs.Unlink(ctx, "/parent/child.txt")
	if err != nil {
		t.Fatalf("Unlink child failed: %v", err)
	}

	// Now delete the parent
	err = ofs.Rmdir(ctx, "/parent")
	if err != nil {
		t.Fatalf("Rmdir parent failed: %v", err)
	}

	// Parent is whiteout, so child should also be blocked
	// (isWhiteout checks ancestors)
	if !ofs.isWhiteout("/parent") {
		t.Error("Expected /parent to be whiteout")
	}
	if !ofs.isWhiteout("/parent/child.txt") {
		t.Error("Expected /parent/child.txt to be blocked by ancestor whiteout")
	}
}

// =============================================================================
// Cache Tests
// =============================================================================

func setupOverlayTestWithCache(t *testing.T) (*OverlayFS, *MockBaseFS, *AgentFS) {
	t.Helper()
	ctx := context.Background()

	// Create AgentFS
	afs, err := Open(ctx, AgentFSOptions{Path: ":memory:"})
	if err != nil {
		t.Fatalf("Failed to open AgentFS: %v", err)
	}

	// Create mock base filesystem
	base := newMockBaseFS()

	// Create overlay filesystem with cache enabled
	ofs := NewOverlayFSWithCache(base, afs.FS, afs.db, OverlayCacheOptions{
		Enabled:    true,
		MaxEntries: 1000,
	})
	if err := ofs.Init(ctx); err != nil {
		afs.Close()
		t.Fatalf("Failed to init OverlayFS: %v", err)
	}

	return ofs, base, afs
}

func TestOverlayCache_BasicCaching(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTestWithCache(t)
	defer afs.Close()

	// Add file to base
	base.addFile(1, "test.txt", 100, []byte("hello"), 0o644)

	// First lookup should miss cache
	_, err := ofs.LookupPath(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("LookupPath failed: %v", err)
	}

	stats := ofs.CacheStats()
	if stats == nil {
		t.Fatal("CacheStats should not be nil when cache is enabled")
	}
	initialMisses := stats.Misses

	// Second lookup should hit cache
	_, err = ofs.LookupPath(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("LookupPath failed: %v", err)
	}

	stats = ofs.CacheStats()
	if stats.Hits == 0 {
		t.Error("Expected cache hit on second lookup")
	}
	if stats.Misses != initialMisses {
		t.Error("Expected no additional misses on second lookup")
	}
}

func TestOverlayCache_InvalidationOnUnlink(t *testing.T) {
	ctx := context.Background()
	ofs, _, afs := setupOverlayTestWithCache(t)
	defer afs.Close()

	// Create a file in delta
	err := ofs.WriteFile(ctx, "/to_delete.txt", []byte("x"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	// Lookup to populate cache
	ofs.LookupPath(ctx, "/to_delete.txt")

	stats := ofs.CacheStats()
	entriesBefore := stats.Entries

	// Delete the file
	err = ofs.Unlink(ctx, "/to_delete.txt")
	if err != nil {
		t.Fatalf("Unlink failed: %v", err)
	}

	// Cache entry should be invalidated
	stats = ofs.CacheStats()
	if stats.Entries >= entriesBefore {
		t.Error("Cache entry should be invalidated after Unlink")
	}

	// Lookup should fail
	_, err = ofs.LookupPath(ctx, "/to_delete.txt")
	if !IsNotExist(err) {
		t.Error("Expected ENOENT after Unlink")
	}
}

func TestOverlayCache_InvalidationOnRename(t *testing.T) {
	ctx := context.Background()
	ofs, _, afs := setupOverlayTestWithCache(t)
	defer afs.Close()

	// Create a file in delta
	err := ofs.WriteFile(ctx, "/old_name.txt", []byte("x"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	// Populate cache
	ofs.LookupPath(ctx, "/old_name.txt")

	// Rename
	err = ofs.Rename(ctx, "/old_name.txt", "/new_name.txt")
	if err != nil {
		t.Fatalf("Rename failed: %v", err)
	}

	// Old path should not exist
	_, err = ofs.LookupPath(ctx, "/old_name.txt")
	if !IsNotExist(err) {
		t.Error("Expected ENOENT for old path after rename")
	}

	// New path should work
	_, err = ofs.LookupPath(ctx, "/new_name.txt")
	if err != nil {
		t.Errorf("New path should exist: %v", err)
	}
}

func TestOverlayCache_InvalidationOnWhiteout(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTestWithCache(t)
	defer afs.Close()

	// Add file to base
	base.addFile(1, "base_file.txt", 100, []byte("hello"), 0o644)

	// Lookup to populate cache
	_, err := ofs.LookupPath(ctx, "/base_file.txt")
	if err != nil {
		t.Fatalf("LookupPath failed: %v", err)
	}

	stats := ofs.CacheStats()
	if stats.Entries == 0 {
		t.Fatal("Expected cache entry after lookup")
	}

	// Delete file (creates whiteout)
	err = ofs.Unlink(ctx, "/base_file.txt")
	if err != nil {
		t.Fatalf("Unlink failed: %v", err)
	}

	// Lookup should fail (whiteout hides base file)
	_, err = ofs.LookupPath(ctx, "/base_file.txt")
	if !IsNotExist(err) {
		t.Error("Expected ENOENT after whiteout created")
	}
}

func TestOverlayCache_ClearCache(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTestWithCache(t)
	defer afs.Close()

	// Add files to base and populate cache
	for i := int64(0); i < 10; i++ {
		name := string(rune('a'+i)) + ".txt"
		base.addFile(1, name, 100+i, []byte("x"), 0o644)
		ofs.LookupPath(ctx, "/"+name)
	}

	stats := ofs.CacheStats()
	if stats.Entries == 0 {
		t.Fatal("Cache should have entries")
	}

	// Clear cache
	ofs.ClearCache()

	stats = ofs.CacheStats()
	if stats.Entries != 0 {
		t.Error("Cache should be empty after ClearCache")
	}
}

func TestOverlayCache_Disabled(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTest(t) // Uses non-cached setup
	defer afs.Close()

	// CacheStats should return nil when cache is disabled
	stats := ofs.CacheStats()
	if stats != nil {
		t.Error("CacheStats should be nil when cache is disabled")
	}

	// Operations should still work
	base.addFile(1, "test.txt", 100, []byte("hello"), 0o644)
	_, err := ofs.LookupPath(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("LookupPath failed: %v", err)
	}
}

func TestOverlayCache_InvalidationOnCopyUp(t *testing.T) {
	ctx := context.Background()
	ofs, base, afs := setupOverlayTestWithCache(t)
	defer afs.Close()

	// Add file to base
	base.addFile(1, "to_modify.txt", 100, []byte("original"), 0o644)

	// Lookup to populate cache
	stats1, err := ofs.LookupPath(ctx, "/to_modify.txt")
	if err != nil {
		t.Fatalf("LookupPath failed: %v", err)
	}
	ino1 := stats1.Ino

	// Modify file (triggers copy-up)
	err = ofs.WriteFile(ctx, "/to_modify.txt", []byte("modified"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	// Lookup after copy-up should return correct data
	stats2, err := ofs.LookupPath(ctx, "/to_modify.txt")
	if err != nil {
		t.Fatalf("LookupPath after copy-up failed: %v", err)
	}

	// Read should return modified content
	content, err := ofs.ReadFile(ctx, "/to_modify.txt")
	if err != nil {
		t.Fatalf("ReadFile failed: %v", err)
	}
	if string(content) != "modified" {
		t.Errorf("Expected 'modified', got %q", string(content))
	}

	// Inode might change after copy-up (implementation detail)
	_ = ino1
	_ = stats2
}

// =============================================================================
// Parameterized Cache Consistency Tests
// =============================================================================
// These tests run the same operations with caching enabled and disabled
// to verify that results are consistent regardless of cache state.

type cacheTestConfig struct {
	name         string
	cacheEnabled bool
}

var cacheConfigs = []cacheTestConfig{
	{"WithCache", true},
	{"WithoutCache", false},
}

func setupOverlayWithConfig(t *testing.T, cfg cacheTestConfig) (*OverlayFS, *MockBaseFS, *AgentFS) {
	t.Helper()
	ctx := context.Background()

	afs, err := Open(ctx, AgentFSOptions{Path: ":memory:"})
	if err != nil {
		t.Fatalf("Failed to open AgentFS: %v", err)
	}

	base := newMockBaseFS()

	var ofs *OverlayFS
	if cfg.cacheEnabled {
		ofs = NewOverlayFSWithCache(base, afs.FS, afs.db, OverlayCacheOptions{
			Enabled:    true,
			MaxEntries: 1000,
		})
	} else {
		ofs = NewOverlayFS(base, afs.FS, afs.db)
	}

	if err := ofs.Init(ctx); err != nil {
		afs.Close()
		t.Fatalf("Failed to init OverlayFS: %v", err)
	}

	return ofs, base, afs
}

// TestOverlayCacheConsistency_FileOperations tests basic file operations
// produce identical results with and without caching.
func TestOverlayCacheConsistency_FileOperations(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Add files to base
			base.addFile(1, "base_file.txt", 100, []byte("base content"), 0o644)
			base.addDir(1, "basedir", 101, 0o755)
			base.addFile(101, "nested.txt", 102, []byte("nested content"), 0o644)

			// Test 1: Read base file
			content, err := ofs.ReadFile(ctx, "/base_file.txt")
			if err != nil {
				t.Fatalf("ReadFile failed: %v", err)
			}
			if string(content) != "base content" {
				t.Errorf("Expected 'base content', got %q", string(content))
			}

			// Test 2: Read base file multiple times (exercises cache)
			for i := 0; i < 5; i++ {
				content, err = ofs.ReadFile(ctx, "/base_file.txt")
				if err != nil {
					t.Fatalf("ReadFile iteration %d failed: %v", i, err)
				}
				if string(content) != "base content" {
					t.Errorf("Iteration %d: Expected 'base content', got %q", i, string(content))
				}
			}

			// Test 3: Write new file
			err = ofs.WriteFile(ctx, "/new_file.txt", []byte("new content"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Test 4: Read new file multiple times
			for i := 0; i < 5; i++ {
				content, err = ofs.ReadFile(ctx, "/new_file.txt")
				if err != nil {
					t.Fatalf("ReadFile new file iteration %d failed: %v", i, err)
				}
				if string(content) != "new content" {
					t.Errorf("Iteration %d: Expected 'new content', got %q", i, string(content))
				}
			}

			// Test 5: Overwrite base file
			err = ofs.WriteFile(ctx, "/base_file.txt", []byte("overwritten"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile overwrite failed: %v", err)
			}
			content, err = ofs.ReadFile(ctx, "/base_file.txt")
			if err != nil {
				t.Fatalf("ReadFile after overwrite failed: %v", err)
			}
			if string(content) != "overwritten" {
				t.Errorf("Expected 'overwritten', got %q", string(content))
			}

			// Test 6: Read nested file
			content, err = ofs.ReadFile(ctx, "/basedir/nested.txt")
			if err != nil {
				t.Fatalf("ReadFile nested failed: %v", err)
			}
			if string(content) != "nested content" {
				t.Errorf("Expected 'nested content', got %q", string(content))
			}
		})
	}
}

// TestOverlayCacheConsistency_DirectoryOperations tests directory operations
// produce identical results with and without caching.
func TestOverlayCacheConsistency_DirectoryOperations(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Add directory structure to base
			base.addDir(1, "basedir", 100, 0o755)
			base.addFile(100, "file1.txt", 101, []byte("f1"), 0o644)
			base.addFile(100, "file2.txt", 102, []byte("f2"), 0o644)

			// Test 1: Readdir on base directory
			entries, err := ofs.Readdir(ctx, 1)
			if err != nil {
				t.Fatalf("Readdir root failed: %v", err)
			}
			if !containsEntry(entries, "basedir") {
				t.Error("Expected 'basedir' in root entries")
			}

			// Test 2: Create new directory
			err = ofs.Mkdir(ctx, "/newdir", 0o755)
			if err != nil {
				t.Fatalf("Mkdir failed: %v", err)
			}

			// Test 3: Readdir should show both directories
			entries, err = ofs.Readdir(ctx, 1)
			if err != nil {
				t.Fatalf("Readdir after mkdir failed: %v", err)
			}
			if !containsEntry(entries, "basedir") || !containsEntry(entries, "newdir") {
				t.Errorf("Expected both 'basedir' and 'newdir', got %v", entries)
			}

			// Test 4: MkdirAll with nested path
			err = ofs.MkdirAll(ctx, "/deep/nested/path", 0o755)
			if err != nil {
				t.Fatalf("MkdirAll failed: %v", err)
			}

			// Test 5: Verify deep path exists
			stats, err := ofs.LookupPath(ctx, "/deep/nested/path")
			if err != nil {
				t.Fatalf("LookupPath deep path failed: %v", err)
			}
			if !stats.IsDir() {
				t.Error("Expected directory")
			}

			// Test 6: Write file in deep path
			err = ofs.WriteFile(ctx, "/deep/nested/path/file.txt", []byte("deep"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile in deep path failed: %v", err)
			}
			content, err := ofs.ReadFile(ctx, "/deep/nested/path/file.txt")
			if err != nil {
				t.Fatalf("ReadFile from deep path failed: %v", err)
			}
			if string(content) != "deep" {
				t.Errorf("Expected 'deep', got %q", string(content))
			}
		})
	}
}

// TestOverlayCacheConsistency_DeleteOperations tests delete operations
// produce identical results with and without caching.
func TestOverlayCacheConsistency_DeleteOperations(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Add files to base and delta
			base.addFile(1, "base_to_delete.txt", 100, []byte("base"), 0o644)
			base.addDir(1, "base_dir", 101, 0o755)
			base.addFile(101, "in_dir.txt", 102, []byte("in dir"), 0o644)

			err := ofs.WriteFile(ctx, "/delta_to_delete.txt", []byte("delta"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Test 1: Lookup files before delete
			_, err = ofs.LookupPath(ctx, "/base_to_delete.txt")
			if err != nil {
				t.Fatalf("LookupPath base file failed: %v", err)
			}
			_, err = ofs.LookupPath(ctx, "/delta_to_delete.txt")
			if err != nil {
				t.Fatalf("LookupPath delta file failed: %v", err)
			}

			// Test 2: Delete base file (creates whiteout)
			err = ofs.Unlink(ctx, "/base_to_delete.txt")
			if err != nil {
				t.Fatalf("Unlink base file failed: %v", err)
			}

			// Test 3: Lookup should fail after delete
			_, err = ofs.LookupPath(ctx, "/base_to_delete.txt")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT after deleting base file")
			}

			// Test 4: Delete delta file
			err = ofs.Unlink(ctx, "/delta_to_delete.txt")
			if err != nil {
				t.Fatalf("Unlink delta file failed: %v", err)
			}
			_, err = ofs.LookupPath(ctx, "/delta_to_delete.txt")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT after deleting delta file")
			}

			// Test 5: Delete file in base directory
			err = ofs.Unlink(ctx, "/base_dir/in_dir.txt")
			if err != nil {
				t.Fatalf("Unlink nested file failed: %v", err)
			}

			// Test 6: Delete base directory (now empty)
			err = ofs.Rmdir(ctx, "/base_dir")
			if err != nil {
				t.Fatalf("Rmdir base dir failed: %v", err)
			}
			_, err = ofs.LookupPath(ctx, "/base_dir")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT after rmdir")
			}
		})
	}
}

// TestOverlayCacheConsistency_RenameOperations tests rename operations
// produce identical results with and without caching.
func TestOverlayCacheConsistency_RenameOperations(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup
			base.addFile(1, "base_rename.txt", 100, []byte("base rename"), 0o644)
			err := ofs.WriteFile(ctx, "/delta_rename.txt", []byte("delta rename"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Lookup files multiple times to populate cache
			for i := 0; i < 3; i++ {
				ofs.LookupPath(ctx, "/base_rename.txt")
				ofs.LookupPath(ctx, "/delta_rename.txt")
			}

			// Test 1: Rename delta file
			err = ofs.Rename(ctx, "/delta_rename.txt", "/delta_renamed.txt")
			if err != nil {
				t.Fatalf("Rename delta file failed: %v", err)
			}

			// Test 2: Old path should not exist
			_, err = ofs.LookupPath(ctx, "/delta_rename.txt")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT for old delta path")
			}

			// Test 3: New path should exist with correct content
			content, err := ofs.ReadFile(ctx, "/delta_renamed.txt")
			if err != nil {
				t.Fatalf("ReadFile renamed delta failed: %v", err)
			}
			if string(content) != "delta rename" {
				t.Errorf("Expected 'delta rename', got %q", string(content))
			}

			// Test 4: Rename base file (triggers copy-up)
			err = ofs.Rename(ctx, "/base_rename.txt", "/base_renamed.txt")
			if err != nil {
				t.Fatalf("Rename base file failed: %v", err)
			}

			// Test 5: Old path should not exist
			_, err = ofs.LookupPath(ctx, "/base_rename.txt")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT for old base path")
			}

			// Test 6: New path should exist with correct content
			content, err = ofs.ReadFile(ctx, "/base_renamed.txt")
			if err != nil {
				t.Fatalf("ReadFile renamed base failed: %v", err)
			}
			if string(content) != "base rename" {
				t.Errorf("Expected 'base rename', got %q", string(content))
			}
		})
	}
}

// TestOverlayCacheConsistency_CopyUpOperations tests copy-up operations
// produce identical results with and without caching.
func TestOverlayCacheConsistency_CopyUpOperations(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Add file to base
			base.addFile(1, "to_modify.txt", 100, []byte("original content"), 0o644)

			// Test 1: Read original content multiple times
			for i := 0; i < 3; i++ {
				content, err := ofs.ReadFile(ctx, "/to_modify.txt")
				if err != nil {
					t.Fatalf("ReadFile iteration %d failed: %v", i, err)
				}
				if string(content) != "original content" {
					t.Errorf("Iteration %d: Expected 'original content', got %q", i, string(content))
				}
			}

			// Test 2: Modify file (triggers copy-up)
			err := ofs.WriteFile(ctx, "/to_modify.txt", []byte("modified content"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile failed: %v", err)
			}

			// Test 3: Read modified content multiple times
			for i := 0; i < 3; i++ {
				content, err := ofs.ReadFile(ctx, "/to_modify.txt")
				if err != nil {
					t.Fatalf("ReadFile after modify iteration %d failed: %v", i, err)
				}
				if string(content) != "modified content" {
					t.Errorf("Iteration %d: Expected 'modified content', got %q", i, string(content))
				}
			}

			// Test 4: Chmod on base file (triggers copy-up)
			base.addFile(1, "to_chmod.txt", 101, []byte("chmod test"), 0o644)

			// Read to populate cache
			ofs.LookupPath(ctx, "/to_chmod.txt")
			ofs.LookupPath(ctx, "/to_chmod.txt")

			err = ofs.Chmod(ctx, "/to_chmod.txt", 0o600)
			if err != nil {
				t.Fatalf("Chmod failed: %v", err)
			}

			// Test 5: Verify chmod was applied
			stats, err := ofs.LookupPath(ctx, "/to_chmod.txt")
			if err != nil {
				t.Fatalf("LookupPath after chmod failed: %v", err)
			}
			// Mode should include file type bits, check permission bits
			if stats.Mode&0o777 != 0o600 {
				t.Errorf("Expected mode 0600, got %o", stats.Mode&0o777)
			}
		})
	}
}

// TestOverlayCacheConsistency_WhiteoutOperations tests whiteout behavior
// is consistent with and without caching.
func TestOverlayCacheConsistency_WhiteoutOperations(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Add files to base
			base.addFile(1, "to_delete.txt", 100, []byte("will be deleted"), 0o644)
			base.addDir(1, "dir_to_delete", 101, 0o755)

			// Test 1: Lookup files multiple times
			for i := 0; i < 3; i++ {
				_, err := ofs.LookupPath(ctx, "/to_delete.txt")
				if err != nil {
					t.Fatalf("LookupPath iteration %d failed: %v", i, err)
				}
			}

			// Test 2: Delete file (creates whiteout)
			err := ofs.Unlink(ctx, "/to_delete.txt")
			if err != nil {
				t.Fatalf("Unlink failed: %v", err)
			}

			// Test 3: Multiple lookups should all fail
			for i := 0; i < 3; i++ {
				_, err = ofs.LookupPath(ctx, "/to_delete.txt")
				if !IsNotExist(err) {
					t.Errorf("Iteration %d: Expected ENOENT after whiteout", i)
				}
			}

			// Test 4: Recreate file at whiteout location
			err = ofs.WriteFile(ctx, "/to_delete.txt", []byte("recreated"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile at whiteout location failed: %v", err)
			}

			// Test 5: File should now exist with new content
			for i := 0; i < 3; i++ {
				content, err := ofs.ReadFile(ctx, "/to_delete.txt")
				if err != nil {
					t.Fatalf("ReadFile iteration %d failed: %v", i, err)
				}
				if string(content) != "recreated" {
					t.Errorf("Iteration %d: Expected 'recreated', got %q", i, string(content))
				}
			}

			// Test 6: Delete and recreate directory
			err = ofs.Rmdir(ctx, "/dir_to_delete")
			if err != nil {
				t.Fatalf("Rmdir failed: %v", err)
			}
			_, err = ofs.LookupPath(ctx, "/dir_to_delete")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT for deleted directory")
			}

			err = ofs.Mkdir(ctx, "/dir_to_delete", 0o755)
			if err != nil {
				t.Fatalf("Mkdir at whiteout location failed: %v", err)
			}
			stats, err := ofs.LookupPath(ctx, "/dir_to_delete")
			if err != nil {
				t.Fatalf("LookupPath recreated dir failed: %v", err)
			}
			if !stats.IsDir() {
				t.Error("Expected directory after mkdir")
			}
		})
	}
}

// TestOverlayCacheConsistency_SymlinkOperations tests symlink operations
// are consistent with and without caching.
func TestOverlayCacheConsistency_SymlinkOperations(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Add symlink to base
			base.addSymlink(1, "base_link", 100, "/target")

			// Test 1: Read base symlink multiple times
			for i := 0; i < 3; i++ {
				target, err := ofs.Readlink(ctx, "/base_link")
				if err != nil {
					t.Fatalf("Readlink iteration %d failed: %v", i, err)
				}
				if target != "/target" {
					t.Errorf("Iteration %d: Expected '/target', got %q", i, target)
				}
			}

			// Test 2: Create delta symlink
			err := ofs.Symlink(ctx, "/new_target", "/delta_link")
			if err != nil {
				t.Fatalf("Symlink failed: %v", err)
			}

			// Test 3: Read delta symlink multiple times
			for i := 0; i < 3; i++ {
				target, err := ofs.Readlink(ctx, "/delta_link")
				if err != nil {
					t.Fatalf("Readlink delta iteration %d failed: %v", i, err)
				}
				if target != "/new_target" {
					t.Errorf("Iteration %d: Expected '/new_target', got %q", i, target)
				}
			}

			// Test 4: Delete base symlink
			err = ofs.Unlink(ctx, "/base_link")
			if err != nil {
				t.Fatalf("Unlink symlink failed: %v", err)
			}
			_, err = ofs.Readlink(ctx, "/base_link")
			if !IsNotExist(err) {
				t.Error("Expected ENOENT after deleting symlink")
			}
		})
	}
}

// TestOverlayCacheConsistency_DeepPathResolution tests deep path resolution
// is consistent with and without caching.
func TestOverlayCacheConsistency_DeepPathResolution(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Create deep directory structure in base
			// /a/b/c/d/e/file.txt
			base.addDir(1, "a", 100, 0o755)
			base.addDir(100, "b", 101, 0o755)
			base.addDir(101, "c", 102, 0o755)
			base.addDir(102, "d", 103, 0o755)
			base.addDir(103, "e", 104, 0o755)
			base.addFile(104, "file.txt", 105, []byte("deep file"), 0o644)

			// Test 1: Lookup deep path multiple times
			for i := 0; i < 5; i++ {
				content, err := ofs.ReadFile(ctx, "/a/b/c/d/e/file.txt")
				if err != nil {
					t.Fatalf("ReadFile deep path iteration %d failed: %v", i, err)
				}
				if string(content) != "deep file" {
					t.Errorf("Iteration %d: Expected 'deep file', got %q", i, string(content))
				}
			}

			// Test 2: Lookup intermediate paths
			paths := []string{"/a", "/a/b", "/a/b/c", "/a/b/c/d", "/a/b/c/d/e"}
			for _, p := range paths {
				for i := 0; i < 3; i++ {
					stats, err := ofs.LookupPath(ctx, p)
					if err != nil {
						t.Fatalf("LookupPath %s iteration %d failed: %v", p, i, err)
					}
					if !stats.IsDir() {
						t.Errorf("Expected %s to be a directory", p)
					}
				}
			}

			// Test 3: Create deep path in delta
			err := ofs.MkdirAll(ctx, "/x/y/z", 0o755)
			if err != nil {
				t.Fatalf("MkdirAll failed: %v", err)
			}
			err = ofs.WriteFile(ctx, "/x/y/z/delta_file.txt", []byte("delta deep"), 0o644)
			if err != nil {
				t.Fatalf("WriteFile in deep delta path failed: %v", err)
			}

			// Test 4: Read from deep delta path multiple times
			for i := 0; i < 5; i++ {
				content, err := ofs.ReadFile(ctx, "/x/y/z/delta_file.txt")
				if err != nil {
					t.Fatalf("ReadFile delta deep path iteration %d failed: %v", i, err)
				}
				if string(content) != "delta deep" {
					t.Errorf("Iteration %d: Expected 'delta deep', got %q", i, string(content))
				}
			}
		})
	}
}

// TestOverlayCacheConsistency_ReaddirMerging tests readdir merging
// is consistent with and without caching.
func TestOverlayCacheConsistency_ReaddirMerging(t *testing.T) {
	for _, cfg := range cacheConfigs {
		t.Run(cfg.name, func(t *testing.T) {
			ctx := context.Background()
			ofs, base, afs := setupOverlayWithConfig(t, cfg)
			defer afs.Close()

			// Setup: Add files to both layers
			base.addFile(1, "base1.txt", 100, []byte("b1"), 0o644)
			base.addFile(1, "base2.txt", 101, []byte("b2"), 0o644)
			base.addFile(1, "common.txt", 102, []byte("base common"), 0o644)

			ofs.WriteFile(ctx, "/delta1.txt", []byte("d1"), 0o644)
			ofs.WriteFile(ctx, "/delta2.txt", []byte("d2"), 0o644)
			ofs.WriteFile(ctx, "/common.txt", []byte("delta common"), 0o644) // Override base

			// Test 1: Readdir should merge both layers
			entries, err := ofs.Readdir(ctx, 1)
			if err != nil {
				t.Fatalf("Readdir failed: %v", err)
			}

			expected := []string{"base1.txt", "base2.txt", "common.txt", "delta1.txt", "delta2.txt"}
			for _, name := range expected {
				if !containsEntry(entries, name) {
					t.Errorf("Expected %q in readdir results", name)
				}
			}

			// Test 2: common.txt should have delta content
			content, err := ofs.ReadFile(ctx, "/common.txt")
			if err != nil {
				t.Fatalf("ReadFile common.txt failed: %v", err)
			}
			if string(content) != "delta common" {
				t.Errorf("Expected 'delta common', got %q", string(content))
			}

			// Test 3: Delete base file
			err = ofs.Unlink(ctx, "/base1.txt")
			if err != nil {
				t.Fatalf("Unlink base1.txt failed: %v", err)
			}

			// Test 4: Readdir should not include deleted file
			entries, err = ofs.Readdir(ctx, 1)
			if err != nil {
				t.Fatalf("Readdir after delete failed: %v", err)
			}
			if containsEntry(entries, "base1.txt") {
				t.Error("Deleted file should not appear in readdir")
			}
		})
	}
}

func containsEntry(entries []string, name string) bool {
	for _, e := range entries {
		if e == name {
			return true
		}
	}
	return false
}
