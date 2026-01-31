package agentfs

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"
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

		stats, err := fs.Stat(ctx, "/symlink.txt")
		if err != nil {
			t.Fatalf("Stat failed: %v", err)
		}
		if !stats.IsSymlink() {
			t.Error("Not a symlink")
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
		f, err := fs.Create(ctx, "/filehandle.txt", 0o644)
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
		f, err := fs.Create(ctx, "/offset.txt", 0o644)
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
		f, err := fs.Create(ctx, "/truncate.txt", 0o644)
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

// setupTestDB creates a temporary database for testing
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
