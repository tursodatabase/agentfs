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
