package agentfs

import (
	"context"
	"fmt"
	"io"
	"math/rand"
	"testing"
)

// ============================================================================
// Benchmark Helpers
// ============================================================================

func setupBenchmarkDB(b *testing.B) *AgentFS {
	b.Helper()
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{Path: ":memory:"})
	if err != nil {
		b.Fatalf("Failed to open AgentFS: %v", err)
	}
	return afs
}

// ============================================================================
// Filesystem - Sequential I/O
// ============================================================================

func BenchmarkSequentialWrite(b *testing.B) {
	sizes := []struct {
		name string
		size int
	}{
		{"4KB", 4 * 1024},
		{"64KB", 64 * 1024},
		{"1MB", 1024 * 1024},
		{"16MB", 16 * 1024 * 1024},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			data := make([]byte, size.size)
			rand.Read(data)

			b.SetBytes(int64(size.size))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				path := fmt.Sprintf("/file_%d.bin", i)
				if err := afs.FS.WriteFile(ctx, path, data, 0o644); err != nil {
					b.Fatalf("WriteFile failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkSequentialRead(b *testing.B) {
	sizes := []struct {
		name string
		size int
	}{
		{"4KB", 4 * 1024},
		{"64KB", 64 * 1024},
		{"1MB", 1024 * 1024},
		{"16MB", 16 * 1024 * 1024},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create test file
			data := make([]byte, size.size)
			rand.Read(data)
			if err := afs.FS.WriteFile(ctx, "/testfile.bin", data, 0o644); err != nil {
				b.Fatalf("WriteFile failed: %v", err)
			}

			b.SetBytes(int64(size.size))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				if _, err := afs.FS.ReadFile(ctx, "/testfile.bin"); err != nil {
					b.Fatalf("ReadFile failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkStreamingWrite(b *testing.B) {
	sizes := []struct {
		name string
		size int
	}{
		{"4KB", 4 * 1024},
		{"64KB", 64 * 1024},
		{"1MB", 1024 * 1024},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			data := make([]byte, size.size)
			rand.Read(data)

			b.SetBytes(int64(size.size))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				path := fmt.Sprintf("/stream_%d.bin", i)
				_, f, err := afs.FS.Create(ctx, path, 0o644)
				if err != nil {
					b.Fatalf("Create failed: %v", err)
				}

				// Write in 4KB chunks like streaming would
				chunkSize := 4096
				for offset := 0; offset < len(data); offset += chunkSize {
					end := offset + chunkSize
					if end > len(data) {
						end = len(data)
					}
					if _, err := f.Write(data[offset:end]); err != nil {
						b.Fatalf("Write failed: %v", err)
					}
				}
				f.Close()
			}
		})
	}
}

func BenchmarkStreamingRead(b *testing.B) {
	sizes := []struct {
		name string
		size int
	}{
		{"4KB", 4 * 1024},
		{"64KB", 64 * 1024},
		{"1MB", 1024 * 1024},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create test file
			data := make([]byte, size.size)
			rand.Read(data)
			if err := afs.FS.WriteFile(ctx, "/testfile.bin", data, 0o644); err != nil {
				b.Fatalf("WriteFile failed: %v", err)
			}

			buf := make([]byte, 4096)
			b.SetBytes(int64(size.size))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				f, err := afs.FS.Open(ctx, "/testfile.bin", O_RDONLY)
				if err != nil {
					b.Fatalf("Open failed: %v", err)
				}

				// Read in 4KB chunks
				for {
					_, err := f.Read(buf)
					if err == io.EOF {
						break
					}
					if err != nil {
						b.Fatalf("Read failed: %v", err)
					}
				}
				f.Close()
			}
		})
	}
}

func BenchmarkStreamingCopy(b *testing.B) {
	sizes := []struct {
		name string
		size int
	}{
		{"64KB", 64 * 1024},
		{"1MB", 1024 * 1024},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create source file
			data := make([]byte, size.size)
			rand.Read(data)
			if err := afs.FS.WriteFile(ctx, "/source.bin", data, 0o644); err != nil {
				b.Fatalf("WriteFile failed: %v", err)
			}

			b.SetBytes(int64(size.size))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				src, err := afs.FS.Open(ctx, "/source.bin", O_RDONLY)
				if err != nil {
					b.Fatalf("Open source failed: %v", err)
				}

				dstPath := fmt.Sprintf("/dest_%d.bin", i)
				_, dst, err := afs.FS.Create(ctx, dstPath, 0o644)
				if err != nil {
					src.Close()
					b.Fatalf("Create dest failed: %v", err)
				}

				if _, err := io.Copy(dst, src); err != nil {
					b.Fatalf("Copy failed: %v", err)
				}

				src.Close()
				dst.Close()
			}
		})
	}
}

// ============================================================================
// Filesystem - Random I/O
// ============================================================================

func BenchmarkRandomRead(b *testing.B) {
	sizes := []struct {
		name     string
		fileSize int
		readSize int
	}{
		{"1MB_file_4KB_read", 1024 * 1024, 4096},
		{"16MB_file_4KB_read", 16 * 1024 * 1024, 4096},
		{"16MB_file_64KB_read", 16 * 1024 * 1024, 64 * 1024},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create test file
			data := make([]byte, size.fileSize)
			rand.Read(data)
			if err := afs.FS.WriteFile(ctx, "/testfile.bin", data, 0o644); err != nil {
				b.Fatalf("WriteFile failed: %v", err)
			}

			f, err := afs.FS.Open(ctx, "/testfile.bin", O_RDONLY)
			if err != nil {
				b.Fatalf("Open failed: %v", err)
			}
			defer f.Close()

			buf := make([]byte, size.readSize)
			maxOffset := size.fileSize - size.readSize

			b.SetBytes(int64(size.readSize))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				offset := int64(rand.Intn(maxOffset))
				if _, err := f.ReadAt(buf, offset); err != nil && err != io.EOF {
					b.Fatalf("ReadAt failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkRandomWrite(b *testing.B) {
	sizes := []struct {
		name      string
		fileSize  int
		writeSize int
	}{
		{"1MB_file_4KB_write", 1024 * 1024, 4096},
		{"16MB_file_4KB_write", 16 * 1024 * 1024, 4096},
		{"16MB_file_64KB_write", 16 * 1024 * 1024, 64 * 1024},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create test file
			data := make([]byte, size.fileSize)
			if err := afs.FS.WriteFile(ctx, "/testfile.bin", data, 0o644); err != nil {
				b.Fatalf("WriteFile failed: %v", err)
			}

			f, err := afs.FS.Open(ctx, "/testfile.bin", O_RDWR)
			if err != nil {
				b.Fatalf("Open failed: %v", err)
			}
			defer f.Close()

			writeData := make([]byte, size.writeSize)
			rand.Read(writeData)
			maxOffset := size.fileSize - size.writeSize

			b.SetBytes(int64(size.writeSize))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				offset := int64(rand.Intn(maxOffset))
				if _, err := f.WriteAt(writeData, offset); err != nil {
					b.Fatalf("WriteAt failed: %v", err)
				}
			}
		})
	}
}

// ============================================================================
// Filesystem - Metadata Operations
// ============================================================================

func BenchmarkStat(b *testing.B) {
	afs := setupBenchmarkDB(b)
	defer afs.Close()
	ctx := context.Background()

	// Create test file
	if err := afs.FS.WriteFile(ctx, "/testfile.txt", []byte("hello"), 0o644); err != nil {
		b.Fatalf("WriteFile failed: %v", err)
	}

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		if _, err := afs.FS.Stat(ctx, "/testfile.txt"); err != nil {
			b.Fatalf("Stat failed: %v", err)
		}
	}
}

func BenchmarkReaddir(b *testing.B) {
	counts := []int{10, 100, 1000}

	for _, count := range counts {
		b.Run(fmt.Sprintf("%d_entries", count), func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create files
			for i := 0; i < count; i++ {
				path := fmt.Sprintf("/file_%04d.txt", i)
				if err := afs.FS.WriteFile(ctx, path, []byte("x"), 0o644); err != nil {
					b.Fatalf("WriteFile failed: %v", err)
				}
			}

			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				if _, err := afs.FS.Readdir(ctx, "/"); err != nil {
					b.Fatalf("Readdir failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkReaddirPlus(b *testing.B) {
	counts := []int{10, 100, 1000}

	for _, count := range counts {
		b.Run(fmt.Sprintf("%d_entries", count), func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create files
			for i := 0; i < count; i++ {
				path := fmt.Sprintf("/file_%04d.txt", i)
				if err := afs.FS.WriteFile(ctx, path, []byte("x"), 0o644); err != nil {
					b.Fatalf("WriteFile failed: %v", err)
				}
			}

			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				if _, err := afs.FS.ReaddirPlus(ctx, "/"); err != nil {
					b.Fatalf("ReaddirPlus failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkMkdir(b *testing.B) {
	afs := setupBenchmarkDB(b)
	defer afs.Close()
	ctx := context.Background()

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		path := fmt.Sprintf("/dir_%d", i)
		if err := afs.FS.Mkdir(ctx, path, 0o755); err != nil {
			b.Fatalf("Mkdir failed: %v", err)
		}
	}
}

func BenchmarkMkdirAll(b *testing.B) {
	depths := []int{2, 5, 10}

	for _, depth := range depths {
		b.Run(fmt.Sprintf("depth_%d", depth), func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				path := fmt.Sprintf("/root_%d", i)
				for d := 0; d < depth; d++ {
					path += fmt.Sprintf("/level_%d", d)
				}
				if err := afs.FS.MkdirAll(ctx, path, 0o755); err != nil {
					b.Fatalf("MkdirAll failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkCreateDelete(b *testing.B) {
	afs := setupBenchmarkDB(b)
	defer afs.Close()
	ctx := context.Background()

	data := []byte("small file content")

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		path := fmt.Sprintf("/file_%d.txt", i)
		if err := afs.FS.WriteFile(ctx, path, data, 0o644); err != nil {
			b.Fatalf("WriteFile failed: %v", err)
		}
		if err := afs.FS.Unlink(ctx, path); err != nil {
			b.Fatalf("Unlink failed: %v", err)
		}
	}
}

func BenchmarkRename(b *testing.B) {
	afs := setupBenchmarkDB(b)
	defer afs.Close()
	ctx := context.Background()

	// Create initial file
	if err := afs.FS.WriteFile(ctx, "/file.txt", []byte("content"), 0o644); err != nil {
		b.Fatalf("WriteFile failed: %v", err)
	}

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		oldPath := "/file.txt"
		newPath := "/renamed.txt"
		if i%2 == 1 {
			oldPath, newPath = newPath, oldPath
		}
		if err := afs.FS.Rename(ctx, oldPath, newPath); err != nil {
			b.Fatalf("Rename failed: %v", err)
		}
	}
}

func BenchmarkChmod(b *testing.B) {
	afs := setupBenchmarkDB(b)
	defer afs.Close()
	ctx := context.Background()

	if err := afs.FS.WriteFile(ctx, "/file.txt", []byte("content"), 0o644); err != nil {
		b.Fatalf("WriteFile failed: %v", err)
	}

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		mode := int64(0o644)
		if i%2 == 1 {
			mode = 0o755
		}
		if err := afs.FS.Chmod(ctx, "/file.txt", mode); err != nil {
			b.Fatalf("Chmod failed: %v", err)
		}
	}
}

// ============================================================================
// Filesystem - Path Resolution
// ============================================================================

func BenchmarkPathResolution(b *testing.B) {
	depths := []int{1, 5, 10, 20}

	for _, depth := range depths {
		b.Run(fmt.Sprintf("depth_%d", depth), func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create deep directory structure
			path := ""
			for d := 0; d < depth; d++ {
				path += fmt.Sprintf("/dir_%d", d)
			}
			if err := afs.FS.MkdirAll(ctx, path, 0o755); err != nil {
				b.Fatalf("MkdirAll failed: %v", err)
			}

			// Create file at the deepest level
			filePath := path + "/file.txt"
			if err := afs.FS.WriteFile(ctx, filePath, []byte("deep"), 0o644); err != nil {
				b.Fatalf("WriteFile failed: %v", err)
			}

			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				if _, err := afs.FS.Stat(ctx, filePath); err != nil {
					b.Fatalf("Stat failed: %v", err)
				}
			}
		})
	}
}

// ============================================================================
// KV Store
// ============================================================================

func BenchmarkKVSet(b *testing.B) {
	sizes := []struct {
		name  string
		value any
	}{
		{"small_string", "hello world"},
		{"medium_string", string(make([]byte, 1024))},
		{"large_string", string(make([]byte, 64*1024))},
		{"small_struct", struct {
			Name string `json:"name"`
			Age  int    `json:"age"`
		}{"Alice", 30}},
		{"large_struct", struct {
			Data [100]int `json:"data"`
		}{}},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				key := fmt.Sprintf("key_%d", i)
				if err := afs.KV.Set(ctx, key, size.value); err != nil {
					b.Fatalf("Set failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkKVGet(b *testing.B) {
	sizes := []struct {
		name  string
		value any
	}{
		{"small_string", "hello world"},
		{"medium_string", string(make([]byte, 1024))},
		{"large_string", string(make([]byte, 64*1024))},
	}

	for _, size := range sizes {
		b.Run(size.name, func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Set initial value
			if err := afs.KV.Set(ctx, "testkey", size.value); err != nil {
				b.Fatalf("Set failed: %v", err)
			}

			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				var result string
				if err := afs.KV.Get(ctx, "testkey", &result); err != nil {
					b.Fatalf("Get failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkKVGetGeneric(b *testing.B) {
	afs := setupBenchmarkDB(b)
	defer afs.Close()
	ctx := context.Background()

	type Config struct {
		Name    string `json:"name"`
		Version int    `json:"version"`
		Debug   bool   `json:"debug"`
	}

	cfg := Config{Name: "myapp", Version: 1, Debug: true}
	if err := afs.KV.Set(ctx, "config", cfg); err != nil {
		b.Fatalf("Set failed: %v", err)
	}

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		if _, err := KVGet[Config](ctx, afs.KV, "config"); err != nil {
			b.Fatalf("KVGet failed: %v", err)
		}
	}
}

func BenchmarkKVHas(b *testing.B) {
	afs := setupBenchmarkDB(b)
	defer afs.Close()
	ctx := context.Background()

	if err := afs.KV.Set(ctx, "exists", "value"); err != nil {
		b.Fatalf("Set failed: %v", err)
	}

	b.ResetTimer()

	for i := 0; i < b.N; i++ {
		if _, err := afs.KV.Has(ctx, "exists"); err != nil {
			b.Fatalf("Has failed: %v", err)
		}
	}
}

func BenchmarkKVKeys(b *testing.B) {
	counts := []int{10, 100, 1000}

	for _, count := range counts {
		b.Run(fmt.Sprintf("%d_keys", count), func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create keys
			for i := 0; i < count; i++ {
				key := fmt.Sprintf("prefix:key_%04d", i)
				if err := afs.KV.Set(ctx, key, i); err != nil {
					b.Fatalf("Set failed: %v", err)
				}
			}

			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				if _, err := afs.KV.Keys(ctx, "prefix:"); err != nil {
					b.Fatalf("Keys failed: %v", err)
				}
			}
		})
	}
}

func BenchmarkKVList(b *testing.B) {
	counts := []int{10, 100, 1000}

	for _, count := range counts {
		b.Run(fmt.Sprintf("%d_keys", count), func(b *testing.B) {
			afs := setupBenchmarkDB(b)
			defer afs.Close()
			ctx := context.Background()

			// Create keys
			for i := 0; i < count; i++ {
				key := fmt.Sprintf("prefix:key_%04d", i)
				if err := afs.KV.Set(ctx, key, i); err != nil {
					b.Fatalf("Set failed: %v", err)
				}
			}

			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				if _, err := afs.KV.List(ctx, "prefix:"); err != nil {
					b.Fatalf("List failed: %v", err)
				}
			}
		})
	}
}

// ============================================================================
// Chunk Size Comparison
// ============================================================================

func BenchmarkChunkSize(b *testing.B) {
	chunkSizes := []int{4096, 16384, 65536}
	fileSize := 1024 * 1024 // 1MB

	for _, chunkSize := range chunkSizes {
		b.Run(fmt.Sprintf("chunk_%dKB_write", chunkSize/1024), func(b *testing.B) {
			ctx := context.Background()
			afs, err := Open(ctx, AgentFSOptions{
				Path:      ":memory:",
				ChunkSize: chunkSize,
			})
			if err != nil {
				b.Fatalf("Failed to open AgentFS: %v", err)
			}
			defer afs.Close()

			data := make([]byte, fileSize)
			rand.Read(data)

			b.SetBytes(int64(fileSize))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				path := fmt.Sprintf("/file_%d.bin", i)
				if err := afs.FS.WriteFile(ctx, path, data, 0o644); err != nil {
					b.Fatalf("WriteFile failed: %v", err)
				}
			}
		})

		b.Run(fmt.Sprintf("chunk_%dKB_read", chunkSize/1024), func(b *testing.B) {
			ctx := context.Background()
			afs, err := Open(ctx, AgentFSOptions{
				Path:      ":memory:",
				ChunkSize: chunkSize,
			})
			if err != nil {
				b.Fatalf("Failed to open AgentFS: %v", err)
			}
			defer afs.Close()

			data := make([]byte, fileSize)
			rand.Read(data)
			if err := afs.FS.WriteFile(ctx, "/testfile.bin", data, 0o644); err != nil {
				b.Fatalf("WriteFile failed: %v", err)
			}

			b.SetBytes(int64(fileSize))
			b.ResetTimer()

			for i := 0; i < b.N; i++ {
				if _, err := afs.FS.ReadFile(ctx, "/testfile.bin"); err != nil {
					b.Fatalf("ReadFile failed: %v", err)
				}
			}
		})
	}
}
