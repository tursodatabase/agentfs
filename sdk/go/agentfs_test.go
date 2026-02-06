package agentfs

import (
	"context"
	"database/sql"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	_ "modernc.org/sqlite"
)

func TestOpen(t *testing.T) {
	ctx := context.Background()

	t.Run("with path", func(t *testing.T) {
		tmpDir := t.TempDir()
		dbPath := filepath.Join(tmpDir, "test.db")

		afs, err := Open(ctx, AgentFSOptions{Path: dbPath})
		if err != nil {
			t.Fatalf("Open failed: %v", err)
		}
		defer afs.Close()

		if afs.Path() != dbPath {
			t.Errorf("Path() = %q, want %q", afs.Path(), dbPath)
		}

		// Verify database file was created
		if _, err := os.Stat(dbPath); os.IsNotExist(err) {
			t.Error("Database file was not created")
		}
	})

	t.Run("with ID", func(t *testing.T) {
		// Create a temp home directory
		tmpHome := t.TempDir()
		oldHome := os.Getenv("HOME")
		os.Setenv("HOME", tmpHome)
		defer os.Setenv("HOME", oldHome)

		afs, err := Open(ctx, AgentFSOptions{ID: "test-agent"})
		if err != nil {
			t.Fatalf("Open failed: %v", err)
		}
		defer afs.Close()

		expectedPath := filepath.Join(tmpHome, ".agentfs", "test-agent.db")
		if afs.Path() != expectedPath {
			t.Errorf("Path() = %q, want %q", afs.Path(), expectedPath)
		}
	})

	t.Run("invalid ID", func(t *testing.T) {
		_, err := Open(ctx, AgentFSOptions{ID: "invalid id with spaces"})
		if err == nil {
			t.Error("Expected error for invalid ID")
		}
	})

	t.Run("no ID or path", func(t *testing.T) {
		_, err := Open(ctx, AgentFSOptions{})
		if err == nil {
			t.Error("Expected error when neither ID nor Path provided")
		}
	})
}

func TestFilesystem(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("root directory exists", func(t *testing.T) {
		stats, err := fs.Stat(ctx, "/")
		if err != nil {
			t.Fatalf("Stat(/) failed: %v", err)
		}
		if !stats.IsDir() {
			t.Error("Root is not a directory")
		}
		if stats.Ino != RootIno {
			t.Errorf("Root ino = %d, want %d", stats.Ino, RootIno)
		}
	})

	t.Run("mkdir and stat", func(t *testing.T) {
		err := fs.Mkdir(ctx, "/testdir", 0o755)
		if err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/testdir")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsDir() {
			t.Error("testdir is not a directory")
		}
		if stats.Permissions() != 0o755 {
			t.Errorf("Permissions = %o, want %o", stats.Permissions(), 0o755)
		}
	})

	t.Run("mkdir existing", func(t *testing.T) {
		err := fs.Mkdir(ctx, "/testdir", 0o755)
		if !IsExist(err) {
			t.Errorf("Expected EEXIST, got %v", err)
		}
	})

	t.Run("mkdirAll", func(t *testing.T) {
		err := fs.MkdirAll(ctx, "/a/b/c/d", 0o755)
		if err != nil {
			t.Fatalf("MkdirAll failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/a/b/c/d")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsDir() {
			t.Error("Not a directory")
		}
	})

	t.Run("writeFile and readFile", func(t *testing.T) {
		content := []byte("Hello, World!")
		err := fs.WriteFile(ctx, "/hello.txt", content, 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		data, err := fs.ReadFile(ctx, "/hello.txt")
		if err != nil {
			t.Fatalf("ReadFile failed: %v", err)
		}
		if string(data) != string(content) {
			t.Errorf("ReadFile = %q, want %q", data, content)
		}

		stats, err := fs.Stat(ctx, "/hello.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsRegularFile() {
			t.Error("Not a regular file")
		}
		if stats.Size != int64(len(content)) {
			t.Errorf("Size = %d, want %d", stats.Size, len(content))
		}
	})

	t.Run("writeFile creates parent directories", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/new/path/to/file.txt", []byte("test"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		_, err = fs.Stat(ctx, "/new/path/to")
		if err != nil {
			t.Errorf("Parent directory not created: %v", err)
		}
	})

	t.Run("readdir", func(t *testing.T) {
		names, err := fs.Readdir(ctx, "/")
		if err != nil {
			t.Fatalf("Readdir failed: %v", err)
		}

		// Should contain at least testdir, hello.txt, a, new
		found := make(map[string]bool)
		for _, name := range names {
			found[name] = true
		}

		expected := []string{"testdir", "hello.txt", "a", "new"}
		for _, name := range expected {
			if !found[name] {
				t.Errorf("Expected %q in directory listing", name)
			}
		}
	})

	t.Run("readdirPlus", func(t *testing.T) {
		entries, err := fs.ReaddirPlus(ctx, "/")
		if err != nil {
			t.Fatalf("ReaddirPlus failed: %v", err)
		}

		for _, entry := range entries {
			if entry.Stats == nil {
				t.Errorf("Entry %q has nil stats", entry.Name)
			}
		}
	})

	t.Run("unlink", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/todelete.txt", []byte("delete me"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err = fs.Unlink(ctx, "/todelete.txt")
		if err != nil {
			t.Fatalf("Unlink failed: %v", err)
		}

		_, err = fs.Stat(ctx, "/todelete.txt")
		if !IsNotExist(err) {
			t.Errorf("Expected ENOENT after unlink, got %v", err)
		}
	})

	t.Run("unlink directory fails", func(t *testing.T) {
		err := fs.Mkdir(ctx, "/cantunlink", 0o755)
		if err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}

		err = fs.Unlink(ctx, "/cantunlink")
		var fsErr *FSError
		if !errors.As(err, &fsErr) || fsErr.Code != EISDIR {
			t.Errorf("Expected EISDIR, got %v", err)
		}
	})

	t.Run("rmdir", func(t *testing.T) {
		err := fs.Mkdir(ctx, "/emptydir", 0o755)
		if err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}

		err = fs.Rmdir(ctx, "/emptydir")
		if err != nil {
			t.Fatalf("Rmdir failed: %v", err)
		}

		_, err = fs.Stat(ctx, "/emptydir")
		if !IsNotExist(err) {
			t.Errorf("Expected ENOENT after rmdir, got %v", err)
		}
	})

	t.Run("rmdir non-empty fails", func(t *testing.T) {
		err := fs.Mkdir(ctx, "/nonempty", 0o755)
		if err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}
		err = fs.WriteFile(ctx, "/nonempty/file.txt", []byte("x"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err = fs.Rmdir(ctx, "/nonempty")
		var fsErr *FSError
		if !errors.As(err, &fsErr) || fsErr.Code != ENOTEMPTY {
			t.Errorf("Expected ENOTEMPTY, got %v", err)
		}
	})

	t.Run("rename file", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/oldname.txt", []byte("content"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err = fs.Rename(ctx, "/oldname.txt", "/newname.txt")
		if err != nil {
			t.Fatalf("Rename failed: %v", err)
		}

		_, err = fs.Stat(ctx, "/oldname.txt")
		if !IsNotExist(err) {
			t.Error("Old path still exists")
		}

		data, err := fs.ReadFile(ctx, "/newname.txt")
		if err != nil {
			t.Fatalf("ReadFile failed: %v", err)
		}
		if string(data) != "content" {
			t.Errorf("Content = %q, want %q", data, "content")
		}
	})

	t.Run("link", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/original.txt", []byte("original"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err = fs.Link(ctx, "/original.txt", "/linked.txt")
		if err != nil {
			t.Fatalf("Link failed: %v", err)
		}

		origStats, _ := fs.Stat(ctx, "/original.txt")
		linkStats, _ := fs.Stat(ctx, "/linked.txt")

		if origStats.Ino != linkStats.Ino {
			t.Error("Hard link has different inode")
		}
		if origStats.Nlink != 2 {
			t.Errorf("Nlink = %d, want 2", origStats.Nlink)
		}
	})

	t.Run("symlink and readlink", func(t *testing.T) {
		err := fs.Symlink(ctx, "/hello.txt", "/symlink.txt")
		if err != nil {
			t.Fatalf("Symlink failed: %v", err)
		}

		// Stat follows the symlink to /hello.txt (a regular file)
		stats, err := fs.Stat(ctx, "/symlink.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsRegularFile() {
			t.Error("Stat on symlink should follow to target (regular file)")
		}

		// Lstat returns the symlink's own stats
		lstats, err := fs.Lstat(ctx, "/symlink.txt")
		if err != nil {
			t.Fatalf("Lstat failed: %v", err)
		}
		if !lstats.IsSymlink() {
			t.Error("Lstat should return symlink stats")
		}

		target, err := fs.Readlink(ctx, "/symlink.txt")
		if err != nil {
			t.Fatalf("Readlink failed: %v", err)
		}
		if target != "/hello.txt" {
			t.Errorf("Target = %q, want %q", target, "/hello.txt")
		}
	})

	t.Run("chmod", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/chmod.txt", []byte("x"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err = fs.Chmod(ctx, "/chmod.txt", 0o600)
		if err != nil {
			t.Fatalf("Chmod failed: %v", err)
		}

		stats, _ := fs.Stat(ctx, "/chmod.txt")
		if stats.Permissions() != 0o600 {
			t.Errorf("Permissions = %o, want %o", stats.Permissions(), 0o600)
		}
	})

	t.Run("utimes", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/utimes.txt", []byte("x"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		atime := int64(1000000)
		mtime := int64(2000000)
		err = fs.Utimes(ctx, "/utimes.txt", atime, mtime)
		if err != nil {
			t.Fatalf("Utimes failed: %v", err)
		}

		stats, _ := fs.Stat(ctx, "/utimes.txt")
		if stats.Atime != atime {
			t.Errorf("Atime = %d, want %d", stats.Atime, atime)
		}
		if stats.Mtime != mtime {
			t.Errorf("Mtime = %d, want %d", stats.Mtime, mtime)
		}
	})
}

func TestNanosecondTimestamps(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("stat includes nanoseconds", func(t *testing.T) {
		before := time.Now()

		err := fs.WriteFile(ctx, "/nsec_test.txt", []byte("test"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		after := time.Now()

		stats, err := fs.Stat(ctx, "/nsec_test.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}

		// Verify seconds are within range
		if stats.Mtime < before.Unix() || stats.Mtime > after.Unix() {
			t.Errorf("Mtime %d not in range [%d, %d]", stats.Mtime, before.Unix(), after.Unix())
		}

		// Verify nanoseconds are valid (0-999999999)
		if stats.MtimeNsec < 0 || stats.MtimeNsec > 999999999 {
			t.Errorf("MtimeNsec %d out of valid range", stats.MtimeNsec)
		}
		if stats.AtimeNsec < 0 || stats.AtimeNsec > 999999999 {
			t.Errorf("AtimeNsec %d out of valid range", stats.AtimeNsec)
		}
		if stats.CtimeNsec < 0 || stats.CtimeNsec > 999999999 {
			t.Errorf("CtimeNsec %d out of valid range", stats.CtimeNsec)
		}

		// Verify helper methods work
		mtimeTime := stats.MtimeTime()
		if mtimeTime.Before(before) || mtimeTime.After(after) {
			t.Errorf("MtimeTime() %v not in range [%v, %v]", mtimeTime, before, after)
		}
	})

	t.Run("UtimesNano sets nanoseconds", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/utimes_nsec.txt", []byte("test"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		// Set specific nanosecond values
		atimeSec := int64(1000000000)
		atimeNsec := int64(123456789)
		mtimeSec := int64(1000000001)
		mtimeNsec := int64(987654321)

		err = fs.UtimesNano(ctx, "/utimes_nsec.txt", atimeSec, atimeNsec, mtimeSec, mtimeNsec)
		if err != nil {
			t.Fatalf("UtimesNano failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/utimes_nsec.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}

		if stats.Atime != atimeSec {
			t.Errorf("Atime = %d, want %d", stats.Atime, atimeSec)
		}
		if stats.AtimeNsec != atimeNsec {
			t.Errorf("AtimeNsec = %d, want %d", stats.AtimeNsec, atimeNsec)
		}
		if stats.Mtime != mtimeSec {
			t.Errorf("Mtime = %d, want %d", stats.Mtime, mtimeSec)
		}
		if stats.MtimeNsec != mtimeNsec {
			t.Errorf("MtimeNsec = %d, want %d", stats.MtimeNsec, mtimeNsec)
		}
	})

	t.Run("readdirPlus includes nanoseconds", func(t *testing.T) {
		err := fs.Mkdir(ctx, "/nsec_dir", 0o755)
		if err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}

		err = fs.WriteFile(ctx, "/nsec_dir/file.txt", []byte("test"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		entries, err := fs.ReaddirPlus(ctx, "/nsec_dir")
		if err != nil {
			t.Fatalf("ReaddirPlus failed: %v", err)
		}

		if len(entries) != 1 {
			t.Fatalf("Expected 1 entry, got %d", len(entries))
		}

		stats := entries[0].Stats
		// Verify nanoseconds are valid
		if stats.MtimeNsec < 0 || stats.MtimeNsec > 999999999 {
			t.Errorf("MtimeNsec %d out of valid range in ReaddirPlus", stats.MtimeNsec)
		}
	})
}

func TestFileHandle(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("pread and pwrite", func(t *testing.T) {
		_, f, err := fs.Create(ctx, "/filehandle.txt", 0o644)
		if err != nil {
			t.Fatalf("Create failed: %v", err)
		}
		defer f.Close()

		// Write some data
		data := []byte("Hello, World!")
		n, err := f.Pwrite(ctx, data, 0)
		if err != nil {
			t.Fatalf("Pwrite failed: %v", err)
		}
		if n != len(data) {
			t.Errorf("Pwrite n = %d, want %d", n, len(data))
		}

		// Read it back
		buf := make([]byte, len(data))
		n, err = f.Pread(ctx, buf, 0)
		if err != nil {
			t.Fatalf("Pread failed: %v", err)
		}
		if n != len(data) {
			t.Errorf("Pread n = %d, want %d", n, len(data))
		}
		if string(buf) != string(data) {
			t.Errorf("Pread = %q, want %q", buf, data)
		}
	})

	t.Run("pwrite at offset", func(t *testing.T) {
		_, f, err := fs.Create(ctx, "/offset.txt", 0o644)
		if err != nil {
			t.Fatalf("Create failed: %v", err)
		}
		defer f.Close()

		// Write "AAAA" at offset 0
		f.Pwrite(ctx, []byte("AAAA"), 0)

		// Write "BB" at offset 10
		f.Pwrite(ctx, []byte("BB"), 10)

		stats, _ := f.Stat(ctx)
		if stats.Size != 12 {
			t.Errorf("Size = %d, want 12", stats.Size)
		}

		// Read all data
		buf := make([]byte, 12)
		f.Pread(ctx, buf, 0)

		// Should be "AAAA" + 6 zeros + "BB"
		expected := append([]byte("AAAA"), make([]byte, 6)...)
		expected = append(expected, []byte("BB")...)
		if string(buf) != string(expected) {
			t.Errorf("Content mismatch: got %v, want %v", buf, expected)
		}
	})

	t.Run("truncate", func(t *testing.T) {
		_, f, err := fs.Create(ctx, "/truncate.txt", 0o644)
		if err != nil {
			t.Fatalf("Create failed: %v", err)
		}
		defer f.Close()

		f.Pwrite(ctx, []byte("Hello, World!"), 0)

		err = f.Truncate(ctx, 5)
		if err != nil {
			t.Fatalf("Truncate failed: %v", err)
		}

		stats, _ := f.Stat(ctx)
		if stats.Size != 5 {
			t.Errorf("Size = %d, want 5", stats.Size)
		}

		buf := make([]byte, 10)
		n, _ := f.Pread(ctx, buf, 0)
		if n != 5 {
			t.Errorf("Read %d bytes, want 5", n)
		}
		if string(buf[:n]) != "Hello" {
			t.Errorf("Content = %q, want %q", buf[:n], "Hello")
		}
	})
}

func TestLargeFile(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	// Create a file larger than one chunk (default 4096 bytes)
	size := 10000
	data := make([]byte, size)
	for i := range data {
		data[i] = byte(i % 256)
	}

	err := fs.WriteFile(ctx, "/large.bin", data, 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	readData, err := fs.ReadFile(ctx, "/large.bin")
	if err != nil {
		t.Fatalf("ReadFile failed: %v", err)
	}

	if len(readData) != size {
		t.Errorf("Size = %d, want %d", len(readData), size)
	}

	for i := range data {
		if readData[i] != data[i] {
			t.Errorf("Byte %d: got %d, want %d", i, readData[i], data[i])
			break
		}
	}
}

func TestKVStore(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	kv := afs.KV

	t.Run("set and get string", func(t *testing.T) {
		err := kv.Set(ctx, "key1", "value1")
		if err != nil {
			t.Fatalf("Set failed: %v", err)
		}

		var value string
		err = kv.Get(ctx, "key1", &value)
		if err != nil {
			t.Fatalf("Get failed: %v", err)
		}
		if value != "value1" {
			t.Errorf("Value = %q, want %q", value, "value1")
		}
	})

	t.Run("set and get struct", func(t *testing.T) {
		type Config struct {
			Name    string `json:"name"`
			Version int    `json:"version"`
		}

		input := Config{Name: "test", Version: 42}
		err := kv.Set(ctx, "config", input)
		if err != nil {
			t.Fatalf("Set failed: %v", err)
		}

		var output Config
		err = kv.Get(ctx, "config", &output)
		if err != nil {
			t.Fatalf("Get failed: %v", err)
		}
		if output != input {
			t.Errorf("Value = %+v, want %+v", output, input)
		}
	})

	t.Run("get non-existent key", func(t *testing.T) {
		var value string
		err := kv.Get(ctx, "nonexistent", &value)
		if err == nil {
			t.Error("Expected error for non-existent key")
		}
	})

	t.Run("has", func(t *testing.T) {
		kv.Set(ctx, "exists", "yes")

		has, err := kv.Has(ctx, "exists")
		if err != nil {
			t.Fatalf("Has failed: %v", err)
		}
		if !has {
			t.Error("Has = false, want true")
		}

		has, err = kv.Has(ctx, "notexists")
		if err != nil {
			t.Fatalf("Has failed: %v", err)
		}
		if has {
			t.Error("Has = true, want false")
		}
	})

	t.Run("delete", func(t *testing.T) {
		kv.Set(ctx, "todelete", "value")

		err := kv.Delete(ctx, "todelete")
		if err != nil {
			t.Fatalf("Delete failed: %v", err)
		}

		has, _ := kv.Has(ctx, "todelete")
		if has {
			t.Error("Key still exists after delete")
		}
	})

	t.Run("keys with prefix", func(t *testing.T) {
		kv.Set(ctx, "prefix:a", "1")
		kv.Set(ctx, "prefix:b", "2")
		kv.Set(ctx, "other:c", "3")

		keys, err := kv.Keys(ctx, "prefix:")
		if err != nil {
			t.Fatalf("Keys failed: %v", err)
		}

		if len(keys) != 2 {
			t.Errorf("Keys count = %d, want 2", len(keys))
		}
	})

	t.Run("list", func(t *testing.T) {
		entries, err := kv.List(ctx, "")
		if err != nil {
			t.Fatalf("List failed: %v", err)
		}

		if len(entries) == 0 {
			t.Error("Expected some entries")
		}

		for _, entry := range entries {
			if entry.CreatedAt == 0 {
				t.Errorf("Entry %q has zero CreatedAt", entry.Key)
			}
		}
	})

	t.Run("clear with prefix", func(t *testing.T) {
		kv.Set(ctx, "clear:a", "1")
		kv.Set(ctx, "clear:b", "2")
		kv.Set(ctx, "keep:c", "3")

		err := kv.Clear(ctx, "clear:")
		if err != nil {
			t.Fatalf("Clear failed: %v", err)
		}

		has, _ := kv.Has(ctx, "clear:a")
		if has {
			t.Error("clear:a still exists")
		}

		has, _ = kv.Has(ctx, "keep:c")
		if !has {
			t.Error("keep:c was deleted")
		}
	})
}

func TestToolCalls(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	tools := afs.Tools

	t.Run("start and success", func(t *testing.T) {
		pending, err := tools.Start(ctx, "test_tool", map[string]string{"arg": "value"})
		if err != nil {
			t.Fatalf("Start failed: %v", err)
		}

		time.Sleep(10 * time.Millisecond) // Small delay for duration

		call, err := pending.Success(ctx, map[string]int{"result": 42})
		if err != nil {
			t.Fatalf("Success failed: %v", err)
		}

		if call.ID == 0 {
			t.Error("Call ID is 0")
		}
		if call.Name != "test_tool" {
			t.Errorf("Name = %q, want %q", call.Name, "test_tool")
		}
		if call.Error != nil {
			t.Error("Error should be nil for success")
		}
	})

	t.Run("start and error", func(t *testing.T) {
		pending, err := tools.Start(ctx, "failing_tool", nil)
		if err != nil {
			t.Fatalf("Start failed: %v", err)
		}

		call, err := pending.Error(ctx, errors.New("something went wrong"))
		if err != nil {
			t.Fatalf("Error failed: %v", err)
		}

		if call.Error == nil {
			t.Error("Error should not be nil")
		}
		if *call.Error != "something went wrong" {
			t.Errorf("Error = %q, want %q", *call.Error, "something went wrong")
		}
	})

	t.Run("record", func(t *testing.T) {
		now := time.Now().Unix()
		call, err := tools.Record(ctx, "direct_tool", map[string]string{"a": "b"}, "result", nil, now-1, now)
		if err != nil {
			t.Fatalf("Record failed: %v", err)
		}

		if call.DurationMs != 1000 {
			t.Errorf("DurationMs = %d, want 1000", call.DurationMs)
		}
	})

	t.Run("get by ID", func(t *testing.T) {
		pending, _ := tools.Start(ctx, "get_test", nil)
		original, _ := pending.Success(ctx, nil)

		retrieved, err := tools.Get(ctx, original.ID)
		if err != nil {
			t.Fatalf("Get failed: %v", err)
		}

		if retrieved.Name != original.Name {
			t.Errorf("Name = %q, want %q", retrieved.Name, original.Name)
		}
	})

	t.Run("get by name", func(t *testing.T) {
		calls, err := tools.GetByName(ctx, "test_tool", 10)
		if err != nil {
			t.Fatalf("GetByName failed: %v", err)
		}

		if len(calls) == 0 {
			t.Error("Expected at least one call")
		}

		for _, call := range calls {
			if call.Name != "test_tool" {
				t.Errorf("Name = %q, want %q", call.Name, "test_tool")
			}
		}
	})

	t.Run("get recent", func(t *testing.T) {
		since := time.Now().Unix() - 60 // Last minute
		calls, err := tools.GetRecent(ctx, since, 100)
		if err != nil {
			t.Fatalf("GetRecent failed: %v", err)
		}

		if len(calls) == 0 {
			t.Error("Expected recent calls")
		}
	})

	t.Run("get stats", func(t *testing.T) {
		stats, err := tools.GetStats(ctx)
		if err != nil {
			t.Fatalf("GetStats failed: %v", err)
		}

		if len(stats) == 0 {
			t.Error("Expected stats")
		}

		// Find test_tool stats
		var found bool
		for _, s := range stats {
			if s.Name == "test_tool" {
				found = true
				if s.TotalCalls == 0 {
					t.Error("TotalCalls is 0")
				}
			}
		}
		if !found {
			t.Error("Stats for test_tool not found")
		}
	})
}

func TestErrors(t *testing.T) {
	t.Run("FSError formatting", func(t *testing.T) {
		err := ErrNoent("stat", "/missing")
		if err.Error() != "stat /missing: no such file or directory" {
			t.Errorf("Error = %q", err.Error())
		}
	})

	t.Run("IsNotExist", func(t *testing.T) {
		err := ErrNoent("stat", "/missing")
		if !IsNotExist(err) {
			t.Error("IsNotExist should return true for ENOENT")
		}

		err2 := ErrExist("mkdir", "/exists")
		if IsNotExist(err2) {
			t.Error("IsNotExist should return false for EEXIST")
		}
	})
}

func TestOpenWith(t *testing.T) {
	ctx := context.Background()

	t.Run("basic usage", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		afs, err := OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith failed: %v", err)
		}

		// Subsystems should be initialized
		if afs.FS == nil {
			t.Fatal("FS is nil")
		}
		if afs.KV == nil {
			t.Fatal("KV is nil")
		}
		if afs.Tools == nil {
			t.Fatal("Tools is nil")
		}

		// Path should be empty for OpenWith
		if afs.Path() != "" {
			t.Errorf("Path() = %q, want empty", afs.Path())
		}

		// DB should return the same connection
		if afs.DB() != db {
			t.Error("DB() did not return the provided connection")
		}
	})

	t.Run("close does not close underlying db", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		afs, err := OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith failed: %v", err)
		}

		// Close the AgentFS
		if err := afs.Close(); err != nil {
			t.Fatalf("Close failed: %v", err)
		}

		// The underlying DB should still be usable
		var result int
		if err := db.QueryRowContext(ctx, "SELECT 1").Scan(&result); err != nil {
			t.Fatalf("DB should still work after AgentFS.Close: %v", err)
		}
		if result != 1 {
			t.Errorf("got %d, want 1", result)
		}
	})

	t.Run("filesystem operations work", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		afs, err := OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith failed: %v", err)
		}

		// Write and read a file
		if err := afs.FS.WriteFile(ctx, "/hello.txt", []byte("hello"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		data, err := afs.FS.ReadFile(ctx, "/hello.txt")
		if err != nil {
			t.Fatalf("ReadFile failed: %v", err)
		}
		if string(data) != "hello" {
			t.Errorf("ReadFile = %q, want %q", data, "hello")
		}
	})

	t.Run("kv operations work", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		afs, err := OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith failed: %v", err)
		}

		if err := afs.KV.Set(ctx, "key1", "value1"); err != nil {
			t.Fatalf("KV.Set failed: %v", err)
		}

		var val string
		if err := afs.KV.Get(ctx, "key1", &val); err != nil {
			t.Fatalf("KV.Get failed: %v", err)
		}
		if val != "value1" {
			t.Errorf("KV.Get = %q, want %q", val, "value1")
		}
	})

	t.Run("tool calls work", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		afs, err := OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith failed: %v", err)
		}

		pending, err := afs.Tools.Start(ctx, "test_tool", nil)
		if err != nil {
			t.Fatalf("Tools.Start failed: %v", err)
		}

		call, err := pending.Success(ctx, "done")
		if err != nil {
			t.Fatalf("Success failed: %v", err)
		}
		if call.Name != "test_tool" {
			t.Errorf("call.Name = %q, want %q", call.Name, "test_tool")
		}
	})

	t.Run("with chunk size option", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		afs, err := OpenWith(ctx, db, WithChunkSize(8192))
		if err != nil {
			t.Fatalf("OpenWith failed: %v", err)
		}

		if afs.FS.ChunkSize() != 8192 {
			t.Errorf("ChunkSize() = %d, want 8192", afs.FS.ChunkSize())
		}
	})

	t.Run("shared database between two AgentFS instances", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		afs1, err := OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith(1) failed: %v", err)
		}

		afs2, err := OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith(2) failed: %v", err)
		}

		// Write from instance 1, read from instance 2
		if err := afs1.FS.WriteFile(ctx, "/shared.txt", []byte("shared"), 0o644); err != nil {
			t.Fatalf("WriteFile via afs1 failed: %v", err)
		}

		data, err := afs2.FS.ReadFile(ctx, "/shared.txt")
		if err != nil {
			t.Fatalf("ReadFile via afs2 failed: %v", err)
		}
		if string(data) != "shared" {
			t.Errorf("ReadFile = %q, want %q", data, "shared")
		}
	})
}

func TestOpenClosesDBOnError(t *testing.T) {
	ctx := context.Background()

	// Open normally first to create a valid database, then tamper with schema version
	tmpDir := t.TempDir()
	dbPath := filepath.Join(tmpDir, "test.db")

	afs, err := Open(ctx, AgentFSOptions{Path: dbPath})
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	afs.Close()

	// Tamper with the schema version
	db, err := sql.Open("sqlite", dbPath)
	if err != nil {
		t.Fatalf("sql.Open failed: %v", err)
	}
	_, err = db.ExecContext(ctx, "UPDATE fs_config SET value = '9.9' WHERE key = 'schema_version'")
	if err != nil {
		t.Fatalf("UPDATE failed: %v", err)
	}
	db.Close()

	// Re-opening should fail with schema version mismatch
	_, err = Open(ctx, AgentFSOptions{Path: dbPath})
	if err == nil {
		t.Fatal("Expected error for schema version mismatch")
	}

	var mismatch *ErrSchemaVersionMismatch
	if !errors.As(err, &mismatch) {
		t.Fatalf("Expected ErrSchemaVersionMismatch, got: %v", err)
	}
}

func TestSchemaVersionValidation(t *testing.T) {
	ctx := context.Background()

	t.Run("fresh database sets schema version", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		_, err = OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith failed: %v", err)
		}

		// Verify schema version was set
		var version string
		if err := db.QueryRowContext(ctx, "SELECT value FROM fs_config WHERE key = 'schema_version'").Scan(&version); err != nil {
			t.Fatalf("Failed to query schema_version: %v", err)
		}
		if version != schemaVersion {
			t.Errorf("schema_version = %q, want %q", version, schemaVersion)
		}
	})

	t.Run("compatible version succeeds", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		// First open initializes
		afs1, err := OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith(1) failed: %v", err)
		}
		_ = afs1

		// Second open should succeed (same version)
		_, err = OpenWith(ctx, db)
		if err != nil {
			t.Fatalf("OpenWith(2) failed: %v", err)
		}
	})

	t.Run("incompatible version fails", func(t *testing.T) {
		db, err := sql.Open("sqlite", ":memory:")
		if err != nil {
			t.Fatalf("sql.Open failed: %v", err)
		}
		defer db.Close()

		// Initialize schema manually with a different version
		if err := initSchema(ctx, db); err != nil {
			t.Fatalf("initSchema failed: %v", err)
		}
		if _, err := db.ExecContext(ctx, "INSERT INTO fs_config (key, value) VALUES ('schema_version', '99.0')"); err != nil {
			t.Fatalf("INSERT failed: %v", err)
		}

		// OpenWith should fail
		_, err = OpenWith(ctx, db)
		if err == nil {
			t.Fatal("Expected error for incompatible schema version")
		}

		var mismatch *ErrSchemaVersionMismatch
		if !errors.As(err, &mismatch) {
			t.Fatalf("Expected ErrSchemaVersionMismatch, got: %T: %v", err, err)
		}
		if mismatch.Found != "99.0" {
			t.Errorf("Found = %q, want %q", mismatch.Found, "99.0")
		}
		if mismatch.Expected != schemaVersion {
			t.Errorf("Expected = %q, want %q", mismatch.Expected, schemaVersion)
		}
	})

	t.Run("error message format", func(t *testing.T) {
		err := &ErrSchemaVersionMismatch{Found: "0.1", Expected: "0.4"}
		msg := err.Error()
		if !strings.Contains(msg, "0.1") || !strings.Contains(msg, "0.4") {
			t.Errorf("Error message missing versions: %q", msg)
		}
	})
}

func TestErrorVariants(t *testing.T) {
	t.Run("ErrNameTooLong", func(t *testing.T) {
		err := ErrNameTooLong("mkdir", "/foo/longname")
		if err.Code != ENAMETOOLONG {
			t.Errorf("Code = %d, want %d (ENAMETOOLONG)", err.Code, ENAMETOOLONG)
		}
		if err.Syscall != "mkdir" {
			t.Errorf("Syscall = %q, want %q", err.Syscall, "mkdir")
		}
		msg := err.Error()
		if !strings.Contains(msg, "file name too long") {
			t.Errorf("Error message should contain 'file name too long': %q", msg)
		}
	})

	t.Run("ErrRootOperation", func(t *testing.T) {
		err := ErrRootOperation("rmdir", "/")
		if err.Code != EPERM {
			t.Errorf("Code = %d, want %d (EPERM)", err.Code, EPERM)
		}
		msg := err.Error()
		if !strings.Contains(msg, "cannot modify root") {
			t.Errorf("Error message should contain 'cannot modify root': %q", msg)
		}
	})

	t.Run("ErrInvalidRename", func(t *testing.T) {
		err := ErrInvalidRename("rename", "/foo")
		if err.Code != EINVAL {
			t.Errorf("Code = %d, want %d (EINVAL)", err.Code, EINVAL)
		}
		msg := err.Error()
		if !strings.Contains(msg, "subtree") {
			t.Errorf("Error message should mention subtree: %q", msg)
		}
	})

	t.Run("ErrNotSymlink", func(t *testing.T) {
		err := ErrNotSymlink("readlink", "/foo")
		if err.Code != EINVAL {
			t.Errorf("Code = %d, want %d (EINVAL)", err.Code, EINVAL)
		}
		msg := err.Error()
		if !strings.Contains(msg, "not a symbolic link") {
			t.Errorf("Error message should mention 'not a symbolic link': %q", msg)
		}
	})

	t.Run("IsNameTooLong", func(t *testing.T) {
		err := ErrNameTooLong("mkdir", "/toolong")
		if !IsNameTooLong(err) {
			t.Error("IsNameTooLong should return true for ENAMETOOLONG errors")
		}
		if IsNameTooLong(ErrNoent("stat", "/foo")) {
			t.Error("IsNameTooLong should return false for ENOENT errors")
		}
	})

	t.Run("ENAMETOOLONG constant", func(t *testing.T) {
		if ENAMETOOLONG != 36 {
			t.Errorf("ENAMETOOLONG = %d, want 36", ENAMETOOLONG)
		}
	})

	t.Run("MaxNameLen constant", func(t *testing.T) {
		if MaxNameLen != 255 {
			t.Errorf("MaxNameLen = %d, want 255", MaxNameLen)
		}
	})

	t.Run("ENAMETOOLONG in codeMessage", func(t *testing.T) {
		err := &FSError{Code: ENAMETOOLONG, Syscall: "test", Path: "/x"}
		msg := err.Error()
		if !strings.Contains(msg, "file name too long") {
			t.Errorf("codeMessage for ENAMETOOLONG should produce 'file name too long': %q", msg)
		}
	})
}

func TestNameLengthValidation(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	longName := strings.Repeat("a", MaxNameLen+1) // 256 bytes

	t.Run("Mkdir rejects long name", func(t *testing.T) {
		err := fs.Mkdir(ctx, "/"+longName, 0o755)
		if err == nil {
			t.Fatal("Expected error for long directory name")
		}
		if !IsNameTooLong(err) {
			t.Errorf("Expected ENAMETOOLONG, got: %v", err)
		}
	})

	t.Run("Mkdir accepts max length name", func(t *testing.T) {
		okName := strings.Repeat("b", MaxNameLen)
		err := fs.Mkdir(ctx, "/"+okName, 0o755)
		if err != nil {
			t.Fatalf("Mkdir with 255-byte name should succeed: %v", err)
		}
	})

	t.Run("WriteFile rejects long name", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/"+longName, []byte("data"), 0o644)
		if err == nil {
			t.Fatal("Expected error for long file name")
		}
		if !IsNameTooLong(err) {
			t.Errorf("Expected ENAMETOOLONG, got: %v", err)
		}
	})

	t.Run("Create rejects long name", func(t *testing.T) {
		_, _, err := fs.Create(ctx, "/"+longName, 0o644)
		if err == nil {
			t.Fatal("Expected error for long file name")
		}
		if !IsNameTooLong(err) {
			t.Errorf("Expected ENAMETOOLONG, got: %v", err)
		}
	})

	t.Run("Link rejects long new name", func(t *testing.T) {
		if err := fs.WriteFile(ctx, "/link_src.txt", []byte("data"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err := fs.Link(ctx, "/link_src.txt", "/"+longName)
		if err == nil {
			t.Fatal("Expected error for long link name")
		}
		if !IsNameTooLong(err) {
			t.Errorf("Expected ENAMETOOLONG, got: %v", err)
		}
	})

	t.Run("Symlink rejects long link name", func(t *testing.T) {
		err := fs.Symlink(ctx, "/target", "/"+longName)
		if err == nil {
			t.Fatal("Expected error for long symlink name")
		}
		if !IsNameTooLong(err) {
			t.Errorf("Expected ENAMETOOLONG, got: %v", err)
		}
	})

	t.Run("Rename rejects long new name", func(t *testing.T) {
		if err := fs.WriteFile(ctx, "/rename_src.txt", []byte("data"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err := fs.Rename(ctx, "/rename_src.txt", "/"+longName)
		if err == nil {
			t.Fatal("Expected error for long new name")
		}
		if !IsNameTooLong(err) {
			t.Errorf("Expected ENAMETOOLONG, got: %v", err)
		}
	})
}

func TestRootOperationGuards(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("Unlink root", func(t *testing.T) {
		err := fs.Unlink(ctx, "/")
		if err == nil {
			t.Fatal("Expected error when unlinking root")
		}
		var fsErr *FSError
		if !errors.As(err, &fsErr) {
			t.Fatalf("Expected FSError, got: %T", err)
		}
		if fsErr.Code != EPERM {
			t.Errorf("Code = %d, want %d (EPERM)", fsErr.Code, EPERM)
		}
		if !strings.Contains(fsErr.Message, "root") {
			t.Errorf("Message should mention root: %q", fsErr.Message)
		}
	})

	t.Run("Rmdir root", func(t *testing.T) {
		err := fs.Rmdir(ctx, "/")
		if err == nil {
			t.Fatal("Expected error when removing root directory")
		}
		var fsErr *FSError
		if !errors.As(err, &fsErr) {
			t.Fatalf("Expected FSError, got: %T", err)
		}
		if fsErr.Code != EPERM {
			t.Errorf("Code = %d, want %d (EPERM)", fsErr.Code, EPERM)
		}
	})

	t.Run("Rename from root", func(t *testing.T) {
		err := fs.Rename(ctx, "/", "/newroot")
		if err == nil {
			t.Fatal("Expected error when renaming root")
		}
		var fsErr *FSError
		if !errors.As(err, &fsErr) {
			t.Fatalf("Expected FSError, got: %T", err)
		}
		if fsErr.Code != EPERM {
			t.Errorf("Code = %d, want %d (EPERM)", fsErr.Code, EPERM)
		}
	})

	t.Run("Rename to root", func(t *testing.T) {
		if err := fs.WriteFile(ctx, "/file.txt", []byte("data"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err := fs.Rename(ctx, "/file.txt", "/")
		if err == nil {
			t.Fatal("Expected error when renaming to root")
		}
	})
}

func TestSubtreeRenamePrevention(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	// Create directory structure: /a/b/c
	if err := fs.MkdirAll(ctx, "/a/b/c", 0o755); err != nil {
		t.Fatalf("MkdirAll failed: %v", err)
	}

	t.Run("rename dir into own subtree", func(t *testing.T) {
		err := fs.Rename(ctx, "/a", "/a/b/new_a")
		if err == nil {
			t.Fatal("Expected error when renaming directory into its own subtree")
		}
		var fsErr *FSError
		if !errors.As(err, &fsErr) {
			t.Fatalf("Expected FSError, got: %T: %v", err, err)
		}
		if fsErr.Code != EINVAL {
			t.Errorf("Code = %d, want %d (EINVAL)", fsErr.Code, EINVAL)
		}
		if !strings.Contains(fsErr.Message, "subtree") {
			t.Errorf("Message should mention subtree: %q", fsErr.Message)
		}
	})

	t.Run("rename dir into deeply nested subtree", func(t *testing.T) {
		err := fs.Rename(ctx, "/a", "/a/b/c/moved")
		if err == nil {
			t.Fatal("Expected error when renaming into deeply nested subtree")
		}
		var fsErr *FSError
		if !errors.As(err, &fsErr) {
			t.Fatalf("Expected FSError, got: %v", err)
		}
	})

	t.Run("rename dir to sibling is allowed", func(t *testing.T) {
		// Create /x/child
		if err := fs.MkdirAll(ctx, "/x/child", 0o755); err != nil {
			t.Fatalf("MkdirAll failed: %v", err)
		}

		// Rename /x to /y should work (sibling, not subtree)
		err := fs.Rename(ctx, "/x", "/y")
		if err != nil {
			t.Fatalf("Rename to sibling should succeed: %v", err)
		}

		// Verify /y/child exists
		stats, err := fs.Stat(ctx, "/y/child")
		if err != nil {
			t.Fatalf("Stat /y/child failed: %v", err)
		}
		if !stats.IsDir() {
			t.Error("/y/child should be a directory")
		}
	})

	t.Run("rename file into dir with same prefix is allowed", func(t *testing.T) {
		// /ab should not be considered a subtree of /a
		if err := fs.Mkdir(ctx, "/ab", 0o755); err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}
		if err := fs.WriteFile(ctx, "/a/test.txt", []byte("test"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err := fs.Rename(ctx, "/a/test.txt", "/ab/test.txt")
		if err != nil {
			t.Fatalf("Rename to /ab/test.txt should succeed (not a subtree of /a): %v", err)
		}
	})
}

func TestReadlinkOnNonSymlink(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("readlink on regular file", func(t *testing.T) {
		if err := fs.WriteFile(ctx, "/regular.txt", []byte("data"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		_, err := fs.Readlink(ctx, "/regular.txt")
		if err == nil {
			t.Fatal("Expected error when readlink on regular file")
		}

		var fsErr *FSError
		if !errors.As(err, &fsErr) {
			t.Fatalf("Expected FSError, got: %T", err)
		}
		if fsErr.Code != EINVAL {
			t.Errorf("Code = %d, want %d (EINVAL)", fsErr.Code, EINVAL)
		}
		if !strings.Contains(fsErr.Message, "not a symbolic link") {
			t.Errorf("Message should mention 'not a symbolic link': %q", fsErr.Message)
		}
	})

	t.Run("readlink on directory", func(t *testing.T) {
		if err := fs.Mkdir(ctx, "/mydir", 0o755); err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}

		_, err := fs.Readlink(ctx, "/mydir")
		if err == nil {
			t.Fatal("Expected error when readlink on directory")
		}
	})

	t.Run("readlink on actual symlink succeeds", func(t *testing.T) {
		if err := fs.Symlink(ctx, "/target", "/mylink"); err != nil {
			t.Fatalf("Symlink failed: %v", err)
		}

		target, err := fs.Readlink(ctx, "/mylink")
		if err != nil {
			t.Fatalf("Readlink failed: %v", err)
		}
		if target != "/target" {
			t.Errorf("Readlink = %q, want %q", target, "/target")
		}
	})
}

func TestLstat(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("regular file", func(t *testing.T) {
		if err := fs.WriteFile(ctx, "/lstat_file.txt", []byte("hello"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		stats, err := fs.Lstat(ctx, "/lstat_file.txt")
		if err != nil {
			t.Fatalf("Lstat failed: %v", err)
		}

		if !stats.IsRegularFile() {
			t.Errorf("expected regular file, got mode %o", stats.Mode)
		}
		if stats.Size != 5 {
			t.Errorf("Size = %d, want 5", stats.Size)
		}
	})

	t.Run("directory", func(t *testing.T) {
		if err := fs.Mkdir(ctx, "/lstat_dir", 0o755); err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}

		stats, err := fs.Lstat(ctx, "/lstat_dir")
		if err != nil {
			t.Fatalf("Lstat failed: %v", err)
		}

		if !stats.IsDir() {
			t.Errorf("expected directory, got mode %o", stats.Mode)
		}
	})

	t.Run("symlink returns symlink stats", func(t *testing.T) {
		if err := fs.Symlink(ctx, "/some_target", "/lstat_link"); err != nil {
			t.Fatalf("Symlink failed: %v", err)
		}

		stats, err := fs.Lstat(ctx, "/lstat_link")
		if err != nil {
			t.Fatalf("Lstat failed: %v", err)
		}

		if !stats.IsSymlink() {
			t.Errorf("expected symlink, got mode %o", stats.Mode)
		}
	})

	t.Run("nonexistent path", func(t *testing.T) {
		_, err := fs.Lstat(ctx, "/no_such_file")
		if err == nil {
			t.Fatal("Expected error for nonexistent path")
		}
		if !IsNotExist(err) {
			t.Errorf("Expected ENOENT, got: %v", err)
		}
	})

	t.Run("matches Stat for regular files", func(t *testing.T) {
		statResult, err := fs.Stat(ctx, "/lstat_file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}

		lstatResult, err := fs.Lstat(ctx, "/lstat_file.txt")
		if err != nil {
			t.Fatalf("Lstat failed: %v", err)
		}

		if statResult.Ino != lstatResult.Ino {
			t.Errorf("Ino mismatch: Stat=%d, Lstat=%d", statResult.Ino, lstatResult.Ino)
		}
		if statResult.Mode != lstatResult.Mode {
			t.Errorf("Mode mismatch: Stat=%o, Lstat=%o", statResult.Mode, lstatResult.Mode)
		}
	})
}

func TestStatfs(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("empty filesystem has root inode", func(t *testing.T) {
		stats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		// At minimum, root inode exists
		if stats.Inodes < 1 {
			t.Errorf("Inodes = %d, want >= 1", stats.Inodes)
		}

		if stats.BytesUsed != 0 {
			t.Errorf("BytesUsed = %d, want 0 for empty filesystem", stats.BytesUsed)
		}
	})

	t.Run("counts inodes correctly", func(t *testing.T) {
		initialStats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		// Create a directory and a file
		if err := fs.Mkdir(ctx, "/statfs_dir", 0o755); err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}
		if err := fs.WriteFile(ctx, "/statfs_file.txt", []byte("content"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		afterStats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		expectedInodes := initialStats.Inodes + 2
		if afterStats.Inodes != expectedInodes {
			t.Errorf("Inodes = %d, want %d", afterStats.Inodes, expectedInodes)
		}
	})

	t.Run("tracks bytes used", func(t *testing.T) {
		if err := fs.WriteFile(ctx, "/statfs_data.bin", []byte("1234567890"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		stats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		// BytesUsed should include the 10 bytes we just wrote plus the 7 from "content"
		if stats.BytesUsed < 17 {
			t.Errorf("BytesUsed = %d, want >= 17", stats.BytesUsed)
		}
	})

	t.Run("bytes decrease after unlink", func(t *testing.T) {
		beforeStats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		if err := fs.Unlink(ctx, "/statfs_data.bin"); err != nil {
			t.Fatalf("Unlink failed: %v", err)
		}

		afterStats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		if afterStats.Inodes >= beforeStats.Inodes {
			t.Errorf("Inodes should decrease after unlink: before=%d, after=%d", beforeStats.Inodes, afterStats.Inodes)
		}
		if afterStats.BytesUsed >= beforeStats.BytesUsed {
			t.Errorf("BytesUsed should decrease after unlink: before=%d, after=%d", beforeStats.BytesUsed, afterStats.BytesUsed)
		}
	})

	t.Run("symlinks count as inodes", func(t *testing.T) {
		beforeStats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		if err := fs.Symlink(ctx, "/target", "/statfs_symlink"); err != nil {
			t.Fatalf("Symlink failed: %v", err)
		}

		afterStats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		if afterStats.Inodes != beforeStats.Inodes+1 {
			t.Errorf("Inodes = %d, want %d", afterStats.Inodes, beforeStats.Inodes+1)
		}
	})
}

func TestChown(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("change uid and gid", func(t *testing.T) {
		if err := fs.WriteFile(ctx, "/chown_file.txt", []byte("data"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		if err := fs.Chown(ctx, "/chown_file.txt", 1000, 2000); err != nil {
			t.Fatalf("Chown failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/chown_file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if stats.UID != 1000 {
			t.Errorf("UID = %d, want 1000", stats.UID)
		}
		if stats.GID != 2000 {
			t.Errorf("GID = %d, want 2000", stats.GID)
		}
	})

	t.Run("change uid only", func(t *testing.T) {
		if err := fs.Chown(ctx, "/chown_file.txt", 500, -1); err != nil {
			t.Fatalf("Chown failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/chown_file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if stats.UID != 500 {
			t.Errorf("UID = %d, want 500", stats.UID)
		}
		if stats.GID != 2000 {
			t.Errorf("GID = %d, want 2000 (unchanged)", stats.GID)
		}
	})

	t.Run("change gid only", func(t *testing.T) {
		if err := fs.Chown(ctx, "/chown_file.txt", -1, 999); err != nil {
			t.Fatalf("Chown failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/chown_file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if stats.UID != 500 {
			t.Errorf("UID = %d, want 500 (unchanged)", stats.UID)
		}
		if stats.GID != 999 {
			t.Errorf("GID = %d, want 999", stats.GID)
		}
	})

	t.Run("both -1 is a no-op", func(t *testing.T) {
		beforeStats, err := fs.Stat(ctx, "/chown_file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}

		if err := fs.Chown(ctx, "/chown_file.txt", -1, -1); err != nil {
			t.Fatalf("Chown(-1, -1) failed: %v", err)
		}

		afterStats, err := fs.Stat(ctx, "/chown_file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}

		if afterStats.UID != beforeStats.UID {
			t.Errorf("UID changed: %d -> %d", beforeStats.UID, afterStats.UID)
		}
		if afterStats.GID != beforeStats.GID {
			t.Errorf("GID changed: %d -> %d", beforeStats.GID, afterStats.GID)
		}
	})

	t.Run("updates ctime", func(t *testing.T) {
		beforeStats, err := fs.Stat(ctx, "/chown_file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}

		time.Sleep(10 * time.Millisecond)

		if err := fs.Chown(ctx, "/chown_file.txt", 42, 42); err != nil {
			t.Fatalf("Chown failed: %v", err)
		}

		afterStats, err := fs.Stat(ctx, "/chown_file.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}

		if afterStats.Ctime < beforeStats.Ctime {
			t.Errorf("Ctime should not decrease: before=%d, after=%d", beforeStats.Ctime, afterStats.Ctime)
		}
		if afterStats.Ctime == beforeStats.Ctime && afterStats.CtimeNsec <= beforeStats.CtimeNsec {
			t.Error("Ctime should have been updated")
		}
	})

	t.Run("chown on directory", func(t *testing.T) {
		if err := fs.Mkdir(ctx, "/chown_dir", 0o755); err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}

		if err := fs.Chown(ctx, "/chown_dir", 100, 200); err != nil {
			t.Fatalf("Chown on directory failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/chown_dir")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if stats.UID != 100 || stats.GID != 200 {
			t.Errorf("UID=%d GID=%d, want UID=100 GID=200", stats.UID, stats.GID)
		}
	})

	t.Run("chown on nonexistent file", func(t *testing.T) {
		err := fs.Chown(ctx, "/no_such_chown_file", 1, 1)
		if err == nil {
			t.Fatal("Expected error for nonexistent file")
		}
		if !IsNotExist(err) {
			t.Errorf("Expected ENOENT, got: %v", err)
		}
	})
}

func TestMknod(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("create FIFO", func(t *testing.T) {
		err := fs.Mknod(ctx, "/my_fifo", S_IFIFO|0o644, 0)
		if err != nil {
			t.Fatalf("Mknod FIFO failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/my_fifo")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsFIFO() {
			t.Errorf("expected FIFO, got mode %o", stats.Mode)
		}
		if stats.Permissions() != 0o644 {
			t.Errorf("Permissions = %o, want 644", stats.Permissions())
		}
		if stats.Nlink != 1 {
			t.Errorf("Nlink = %d, want 1", stats.Nlink)
		}
	})

	t.Run("create character device", func(t *testing.T) {
		err := fs.Mknod(ctx, "/my_chardev", S_IFCHR|0o660, 0x0501) // major=5, minor=1
		if err != nil {
			t.Fatalf("Mknod char device failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/my_chardev")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsCharDevice() {
			t.Errorf("expected char device, got mode %o", stats.Mode)
		}
		if stats.Rdev != 0x0501 {
			t.Errorf("Rdev = %d, want %d", stats.Rdev, 0x0501)
		}
	})

	t.Run("create block device", func(t *testing.T) {
		err := fs.Mknod(ctx, "/my_blkdev", S_IFBLK|0o660, 0x0800) // major=8, minor=0
		if err != nil {
			t.Fatalf("Mknod block device failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/my_blkdev")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsBlockDevice() {
			t.Errorf("expected block device, got mode %o", stats.Mode)
		}
		if stats.Rdev != 0x0800 {
			t.Errorf("Rdev = %d, want %d", stats.Rdev, 0x0800)
		}
	})

	t.Run("create socket", func(t *testing.T) {
		err := fs.Mknod(ctx, "/my_sock", S_IFSOCK|0o755, 0)
		if err != nil {
			t.Fatalf("Mknod socket failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/my_sock")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsSocket() {
			t.Errorf("expected socket, got mode %o", stats.Mode)
		}
	})

	t.Run("rejects invalid mode without special file type", func(t *testing.T) {
		// S_IFREG is not valid for mknod
		err := fs.Mknod(ctx, "/bad_mode", S_IFREG|0o644, 0)
		if err == nil {
			t.Fatal("Expected error for invalid mode")
		}
		var fsErr *FSError
		if !errors.As(err, &fsErr) {
			t.Fatalf("Expected FSError, got: %T", err)
		}
		if fsErr.Code != EINVAL {
			t.Errorf("Code = %d, want %d (EINVAL)", fsErr.Code, EINVAL)
		}
	})

	t.Run("rejects directory mode", func(t *testing.T) {
		err := fs.Mknod(ctx, "/bad_dir", S_IFDIR|0o755, 0)
		if err == nil {
			t.Fatal("Expected error for directory mode")
		}
	})

	t.Run("rejects symlink mode", func(t *testing.T) {
		err := fs.Mknod(ctx, "/bad_symlink", S_IFLNK|0o777, 0)
		if err == nil {
			t.Fatal("Expected error for symlink mode")
		}
	})

	t.Run("rejects plain permission bits (no file type)", func(t *testing.T) {
		err := fs.Mknod(ctx, "/no_type", 0o644, 0)
		if err == nil {
			t.Fatal("Expected error when no file type in mode")
		}
	})

	t.Run("already exists", func(t *testing.T) {
		err := fs.Mknod(ctx, "/my_fifo", S_IFIFO|0o644, 0)
		if err == nil {
			t.Fatal("Expected EEXIST error")
		}
		if !IsExist(err) {
			t.Errorf("Expected EEXIST, got: %v", err)
		}
	})

	t.Run("parent not found", func(t *testing.T) {
		err := fs.Mknod(ctx, "/no_parent/fifo", S_IFIFO|0o644, 0)
		if err == nil {
			t.Fatal("Expected error for nonexistent parent")
		}
		if !IsNotExist(err) {
			t.Errorf("Expected ENOENT, got: %v", err)
		}
	})

	t.Run("parent not a directory", func(t *testing.T) {
		if err := fs.WriteFile(ctx, "/mknod_file", []byte("data"), 0o644); err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err := fs.Mknod(ctx, "/mknod_file/fifo", S_IFIFO|0o644, 0)
		if err == nil {
			t.Fatal("Expected error when parent is not a directory")
		}
	})

	t.Run("root path rejected", func(t *testing.T) {
		err := fs.Mknod(ctx, "/", S_IFIFO|0o644, 0)
		if err == nil {
			t.Fatal("Expected error for root path")
		}
		var fsErr *FSError
		if !errors.As(err, &fsErr) {
			t.Fatalf("Expected FSError, got: %T", err)
		}
		if fsErr.Code != EPERM {
			t.Errorf("Code = %d, want %d (EPERM)", fsErr.Code, EPERM)
		}
	})

	t.Run("name too long", func(t *testing.T) {
		longName := strings.Repeat("x", MaxNameLen+1)
		err := fs.Mknod(ctx, "/"+longName, S_IFIFO|0o644, 0)
		if err == nil {
			t.Fatal("Expected ENAMETOOLONG")
		}
		if !IsNameTooLong(err) {
			t.Errorf("Expected ENAMETOOLONG, got: %v", err)
		}
	})

	t.Run("create in subdirectory", func(t *testing.T) {
		if err := fs.Mkdir(ctx, "/mknod_subdir", 0o755); err != nil {
			t.Fatalf("Mkdir failed: %v", err)
		}

		err := fs.Mknod(ctx, "/mknod_subdir/pipe", S_IFIFO|0o644, 0)
		if err != nil {
			t.Fatalf("Mknod in subdir failed: %v", err)
		}

		stats, err := fs.Stat(ctx, "/mknod_subdir/pipe")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsFIFO() {
			t.Errorf("expected FIFO, got mode %o", stats.Mode)
		}
	})

	t.Run("visible in readdir", func(t *testing.T) {
		entries, err := fs.Readdir(ctx, "/mknod_subdir")
		if err != nil {
			t.Fatalf("Readdir failed: %v", err)
		}

		found := false
		for _, name := range entries {
			if name == "pipe" {
				found = true
				break
			}
		}
		if !found {
			t.Errorf("mknod entry 'pipe' not found in readdir results: %v", entries)
		}
	})

	t.Run("statfs counts mknod inodes", func(t *testing.T) {
		beforeStats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		if err := fs.Mknod(ctx, "/counted_fifo", S_IFIFO|0o644, 0); err != nil {
			t.Fatalf("Mknod failed: %v", err)
		}

		afterStats, err := fs.Statfs(ctx)
		if err != nil {
			t.Fatalf("Statfs failed: %v", err)
		}

		if afterStats.Inodes != beforeStats.Inodes+1 {
			t.Errorf("Inodes = %d, want %d", afterStats.Inodes, beforeStats.Inodes+1)
		}
	})
}

// setupTestDB creates a temporary database for testing
func TestUtimens(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("TimeSet sets both timestamps", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/utimens_set.txt", []byte("x"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		err = fs.Utimens(ctx, "/utimens_set.txt",
			TimeSet(1000, 123456789),
			TimeSet(2000, 987654321),
		)
		if err != nil {
			t.Fatalf("Utimens failed: %v", err)
		}

		stats, _ := fs.Stat(ctx, "/utimens_set.txt")
		if stats.Atime != 1000 {
			t.Errorf("Atime = %d, want 1000", stats.Atime)
		}
		if stats.AtimeNsec != 123456789 {
			t.Errorf("AtimeNsec = %d, want 123456789", stats.AtimeNsec)
		}
		if stats.Mtime != 2000 {
			t.Errorf("Mtime = %d, want 2000", stats.Mtime)
		}
		if stats.MtimeNsec != 987654321 {
			t.Errorf("MtimeNsec = %d, want 987654321", stats.MtimeNsec)
		}
	})

	t.Run("TimeOmit leaves timestamp unchanged", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/utimens_omit.txt", []byte("x"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		// Set known values first
		err = fs.Utimens(ctx, "/utimens_omit.txt",
			TimeSet(1000, 111),
			TimeSet(2000, 222),
		)
		if err != nil {
			t.Fatalf("Utimens failed: %v", err)
		}

		// Now omit atime, change mtime
		err = fs.Utimens(ctx, "/utimens_omit.txt",
			TimeOmit(),
			TimeSet(3000, 333),
		)
		if err != nil {
			t.Fatalf("Utimens failed: %v", err)
		}

		stats, _ := fs.Stat(ctx, "/utimens_omit.txt")
		if stats.Atime != 1000 || stats.AtimeNsec != 111 {
			t.Errorf("Atime changed: %d.%d, want 1000.111", stats.Atime, stats.AtimeNsec)
		}
		if stats.Mtime != 3000 || stats.MtimeNsec != 333 {
			t.Errorf("Mtime = %d.%d, want 3000.333", stats.Mtime, stats.MtimeNsec)
		}
	})

	t.Run("TimeOmit both is no-op", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/utimens_noop.txt", []byte("x"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		statsBefore, _ := fs.Stat(ctx, "/utimens_noop.txt")

		err = fs.Utimens(ctx, "/utimens_noop.txt", TimeOmit(), TimeOmit())
		if err != nil {
			t.Fatalf("Utimens failed: %v", err)
		}

		statsAfter, _ := fs.Stat(ctx, "/utimens_noop.txt")
		if statsBefore.Atime != statsAfter.Atime || statsBefore.AtimeNsec != statsAfter.AtimeNsec {
			t.Error("Atime changed on double-omit")
		}
		if statsBefore.Mtime != statsAfter.Mtime || statsBefore.MtimeNsec != statsAfter.MtimeNsec {
			t.Error("Mtime changed on double-omit")
		}
		// ctime should also be unchanged since we short-circuit
		if statsBefore.Ctime != statsAfter.Ctime || statsBefore.CtimeNsec != statsAfter.CtimeNsec {
			t.Error("Ctime changed on double-omit")
		}
	})

	t.Run("TimeNow sets to current time", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/utimens_now.txt", []byte("x"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		// Set atime to old value
		err = fs.Utimens(ctx, "/utimens_now.txt",
			TimeSet(1000, 0),
			TimeSet(1000, 0),
		)
		if err != nil {
			t.Fatalf("Utimens failed: %v", err)
		}

		before := time.Now()
		err = fs.Utimens(ctx, "/utimens_now.txt", TimeNow(), TimeNow())
		if err != nil {
			t.Fatalf("Utimens failed: %v", err)
		}
		after := time.Now()

		stats, _ := fs.Stat(ctx, "/utimens_now.txt")
		if stats.Atime < before.Unix() || stats.Atime > after.Unix() {
			t.Errorf("Atime %d not in range [%d, %d]", stats.Atime, before.Unix(), after.Unix())
		}
		if stats.Mtime < before.Unix() || stats.Mtime > after.Unix() {
			t.Errorf("Mtime %d not in range [%d, %d]", stats.Mtime, before.Unix(), after.Unix())
		}
	})

	t.Run("Utimens updates ctime", func(t *testing.T) {
		err := fs.WriteFile(ctx, "/utimens_ctime.txt", []byte("x"), 0o644)
		if err != nil {
			t.Fatalf("WriteFile failed: %v", err)
		}

		before := time.Now()
		err = fs.Utimens(ctx, "/utimens_ctime.txt",
			TimeSet(5000, 0),
			TimeOmit(),
		)
		if err != nil {
			t.Fatalf("Utimens failed: %v", err)
		}
		after := time.Now()

		stats, _ := fs.Stat(ctx, "/utimens_ctime.txt")
		if stats.Ctime < before.Unix() || stats.Ctime > after.Unix() {
			t.Errorf("Ctime %d not in range [%d, %d]", stats.Ctime, before.Unix(), after.Unix())
		}
	})

	t.Run("Utimens on nonexistent path", func(t *testing.T) {
		err := fs.Utimens(ctx, "/no_such_file.txt", TimeNow(), TimeNow())
		if err == nil {
			t.Fatal("Expected error")
		}
		if !IsNotExist(err) {
			t.Errorf("Expected ENOENT, got: %v", err)
		}
	})

	t.Run("TimeChange constructors", func(t *testing.T) {
		omit := TimeOmit()
		if !omit.IsOmit() || omit.IsNow() || omit.IsSet() {
			t.Error("TimeOmit() should be Omit")
		}

		now := TimeNow()
		if now.IsOmit() || !now.IsNow() || now.IsSet() {
			t.Error("TimeNow() should be Now")
		}

		set := TimeSet(100, 200)
		if set.IsOmit() || set.IsNow() || !set.IsSet() {
			t.Error("TimeSet() should be Set")
		}
	})
}

func TestCreateReturnsStats(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()
	fs := afs.FS

	t.Run("Create returns valid stats", func(t *testing.T) {
		before := time.Now()
		stats, f, err := fs.Create(ctx, "/created.txt", 0o644)
		if err != nil {
			t.Fatalf("Create failed: %v", err)
		}
		defer f.Close()
		after := time.Now()

		if stats == nil {
			t.Fatal("Stats should not be nil")
		}
		if !stats.IsRegularFile() {
			t.Errorf("Mode = %o, want regular file", stats.Mode)
		}
		if stats.Permissions() != 0o644 {
			t.Errorf("Permissions = %o, want 0644", stats.Permissions())
		}
		if stats.Size != 0 {
			t.Errorf("Size = %d, want 0", stats.Size)
		}
		if stats.Ino <= 0 {
			t.Errorf("Ino = %d, want > 0", stats.Ino)
		}
		if stats.Nlink != 1 {
			t.Errorf("Nlink = %d, want 1", stats.Nlink)
		}
		if stats.Ctime < before.Unix() || stats.Ctime > after.Unix() {
			t.Errorf("Ctime %d not in range [%d, %d]", stats.Ctime, before.Unix(), after.Unix())
		}
	})

	t.Run("Create stats match subsequent Stat", func(t *testing.T) {
		createStats, f, err := fs.Create(ctx, "/stat_match.txt", 0o600)
		if err != nil {
			t.Fatalf("Create failed: %v", err)
		}
		f.Close()

		statStats, err := fs.Stat(ctx, "/stat_match.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}

		if createStats.Ino != statStats.Ino {
			t.Errorf("Ino: Create=%d, Stat=%d", createStats.Ino, statStats.Ino)
		}
		if createStats.Mode != statStats.Mode {
			t.Errorf("Mode: Create=%o, Stat=%o", createStats.Mode, statStats.Mode)
		}
		if createStats.Size != statStats.Size {
			t.Errorf("Size: Create=%d, Stat=%d", createStats.Size, statStats.Size)
		}
	})

	t.Run("Create file handle is usable", func(t *testing.T) {
		_, f, err := fs.Create(ctx, "/usable.txt", 0o644)
		if err != nil {
			t.Fatalf("Create failed: %v", err)
		}
		defer f.Close()

		n, err := f.Pwrite(ctx, []byte("hello"), 0)
		if err != nil {
			t.Fatalf("Pwrite failed: %v", err)
		}
		if n != 5 {
			t.Errorf("Pwrite n = %d, want 5", n)
		}

		buf := make([]byte, 5)
		n, err = f.Pread(ctx, buf, 0)
		if err != nil {
			t.Fatalf("Pread failed: %v", err)
		}
		if string(buf[:n]) != "hello" {
			t.Errorf("Content = %q, want %q", string(buf[:n]), "hello")
		}
	})
}

func setupTestDB(t *testing.T) *AgentFS {
	t.Helper()
	ctx := context.Background()
	tmpDir := t.TempDir()
	dbPath := filepath.Join(tmpDir, "test.db")

	afs, err := Open(ctx, AgentFSOptions{Path: dbPath})
	if err != nil {
		t.Fatalf("Failed to create test database: %v", err)
	}

	return afs
}
