package agentfs

import (
	"bytes"
	"context"
	"io"
	"testing"
)

func TestFile_IOReader(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	content := []byte("Hello, World! This is a test file for streaming I/O.")
	afs.FS.WriteFile(ctx, "/stream.txt", content, 0o644)

	t.Run("Read entire file", func(t *testing.T) {
		f, err := afs.FS.Open(ctx, "/stream.txt", O_RDONLY)
		if err != nil {
			t.Fatalf("Open failed: %v", err)
		}
		defer f.Close()

		data, err := io.ReadAll(f)
		if err != nil {
			t.Fatalf("ReadAll failed: %v", err)
		}
		if string(data) != string(content) {
			t.Errorf("Content = %q, want %q", data, content)
		}
	})

	t.Run("Read in chunks", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/stream.txt", O_RDONLY)
		defer f.Close()

		buf := make([]byte, 10)
		var result []byte

		for {
			n, err := f.Read(buf)
			result = append(result, buf[:n]...)
			if err == io.EOF {
				break
			}
			if err != nil {
				t.Fatalf("Read failed: %v", err)
			}
		}

		if string(result) != string(content) {
			t.Errorf("Content = %q, want %q", result, content)
		}
	})

	t.Run("Read returns EOF at end", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/stream.txt", O_RDONLY)
		defer f.Close()

		// Read entire file
		io.ReadAll(f)

		// Next read should return EOF
		buf := make([]byte, 10)
		n, err := f.Read(buf)
		if n != 0 {
			t.Errorf("n = %d, want 0", n)
		}
		if err != io.EOF {
			t.Errorf("err = %v, want io.EOF", err)
		}
	})

	t.Run("Offset advances", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/stream.txt", O_RDONLY)
		defer f.Close()

		if f.Offset() != 0 {
			t.Errorf("Initial offset = %d, want 0", f.Offset())
		}

		buf := make([]byte, 5)
		f.Read(buf)

		if f.Offset() != 5 {
			t.Errorf("Offset after read = %d, want 5", f.Offset())
		}
	})
}

func TestFile_IOWriter(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	t.Run("Write entire file", func(t *testing.T) {
		_, f, err := afs.FS.Create(ctx, "/write.txt", 0o644)
		if err != nil {
			t.Fatalf("Create failed: %v", err)
		}

		content := []byte("Hello from streaming write!")
		n, err := f.Write(content)
		if err != nil {
			t.Fatalf("Write failed: %v", err)
		}
		if n != len(content) {
			t.Errorf("n = %d, want %d", n, len(content))
		}
		f.Close()

		// Verify
		data, _ := afs.FS.ReadFile(ctx, "/write.txt")
		if string(data) != string(content) {
			t.Errorf("Content = %q, want %q", data, content)
		}
	})

	t.Run("Write in chunks", func(t *testing.T) {
		_, f, _ := afs.FS.Create(ctx, "/chunks.txt", 0o644)
		defer f.Close()

		chunks := []string{"Hello, ", "World! ", "This is ", "chunked."}
		for _, chunk := range chunks {
			f.Write([]byte(chunk))
		}

		data, _ := afs.FS.ReadFile(ctx, "/chunks.txt")
		expected := "Hello, World! This is chunked."
		if string(data) != expected {
			t.Errorf("Content = %q, want %q", data, expected)
		}
	})

	t.Run("Offset advances on write", func(t *testing.T) {
		_, f, _ := afs.FS.Create(ctx, "/offset.txt", 0o644)
		defer f.Close()

		if f.Offset() != 0 {
			t.Errorf("Initial offset = %d, want 0", f.Offset())
		}

		f.Write([]byte("12345"))
		if f.Offset() != 5 {
			t.Errorf("Offset after write = %d, want 5", f.Offset())
		}

		f.Write([]byte("67890"))
		if f.Offset() != 10 {
			t.Errorf("Offset after second write = %d, want 10", f.Offset())
		}
	})
}

func TestFile_IOSeeker(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	content := []byte("0123456789ABCDEF")
	afs.FS.WriteFile(ctx, "/seek.txt", content, 0o644)

	t.Run("SeekStart", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/seek.txt", O_RDONLY)
		defer f.Close()

		pos, err := f.Seek(5, io.SeekStart)
		if err != nil {
			t.Fatalf("Seek failed: %v", err)
		}
		if pos != 5 {
			t.Errorf("pos = %d, want 5", pos)
		}

		buf := make([]byte, 5)
		f.Read(buf)
		if string(buf) != "56789" {
			t.Errorf("Read = %q, want %q", buf, "56789")
		}
	})

	t.Run("SeekCurrent", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/seek.txt", O_RDONLY)
		defer f.Close()

		f.Seek(5, io.SeekStart)
		pos, err := f.Seek(3, io.SeekCurrent)
		if err != nil {
			t.Fatalf("Seek failed: %v", err)
		}
		if pos != 8 {
			t.Errorf("pos = %d, want 8", pos)
		}

		// Negative seek
		pos, _ = f.Seek(-2, io.SeekCurrent)
		if pos != 6 {
			t.Errorf("pos = %d, want 6", pos)
		}
	})

	t.Run("SeekEnd", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/seek.txt", O_RDONLY)
		defer f.Close()

		pos, err := f.Seek(-5, io.SeekEnd)
		if err != nil {
			t.Fatalf("Seek failed: %v", err)
		}
		if pos != 11 {
			t.Errorf("pos = %d, want 11", pos)
		}

		buf := make([]byte, 5)
		f.Read(buf)
		if string(buf) != "BCDEF" {
			t.Errorf("Read = %q, want %q", buf, "BCDEF")
		}
	})

	t.Run("Seek to beginning", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/seek.txt", O_RDONLY)
		defer f.Close()

		// Read some data
		buf := make([]byte, 10)
		f.Read(buf)

		// Seek back to start
		pos, _ := f.Seek(0, io.SeekStart)
		if pos != 0 {
			t.Errorf("pos = %d, want 0", pos)
		}

		// Read from beginning
		buf = make([]byte, 5)
		f.Read(buf)
		if string(buf) != "01234" {
			t.Errorf("Read = %q, want %q", buf, "01234")
		}
	})

	t.Run("Negative offset error", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/seek.txt", O_RDONLY)
		defer f.Close()

		_, err := f.Seek(-10, io.SeekStart)
		if err == nil {
			t.Error("Expected error for negative offset")
		}
	})

	t.Run("Invalid whence error", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/seek.txt", O_RDONLY)
		defer f.Close()

		_, err := f.Seek(0, 99)
		if err == nil {
			t.Error("Expected error for invalid whence")
		}
	})
}

func TestFile_IOReaderAt(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	content := []byte("0123456789ABCDEF")
	afs.FS.WriteFile(ctx, "/readat.txt", content, 0o644)

	t.Run("ReadAt various offsets", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/readat.txt", O_RDONLY)
		defer f.Close()

		tests := []struct {
			offset int64
			len    int
			want   string
		}{
			{0, 5, "01234"},
			{5, 5, "56789"},
			{10, 6, "ABCDEF"},
			{0, 16, "0123456789ABCDEF"},
		}

		for _, tc := range tests {
			buf := make([]byte, tc.len)
			n, err := f.ReadAt(buf, tc.offset)
			if err != nil && err != io.EOF {
				t.Errorf("ReadAt(%d) failed: %v", tc.offset, err)
			}
			if string(buf[:n]) != tc.want {
				t.Errorf("ReadAt(%d) = %q, want %q", tc.offset, buf[:n], tc.want)
			}
		}
	})

	t.Run("ReadAt does not affect offset", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/readat.txt", O_RDONLY)
		defer f.Close()

		initialOffset := f.Offset()

		buf := make([]byte, 5)
		f.ReadAt(buf, 10)

		if f.Offset() != initialOffset {
			t.Errorf("Offset changed from %d to %d", initialOffset, f.Offset())
		}
	})

	t.Run("ReadAt returns EOF for short read", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/readat.txt", O_RDONLY)
		defer f.Close()

		buf := make([]byte, 10)
		n, err := f.ReadAt(buf, 12) // Only 4 bytes available

		if n != 4 {
			t.Errorf("n = %d, want 4", n)
		}
		if err != io.EOF {
			t.Errorf("err = %v, want io.EOF", err)
		}
	})
}

func TestFile_IOWriterAt(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	t.Run("WriteAt various offsets", func(t *testing.T) {
		_, f, _ := afs.FS.Create(ctx, "/writeat.txt", 0o644)
		defer f.Close()

		// Write "HELLO" at offset 0
		f.WriteAt([]byte("HELLO"), 0)

		// Write "WORLD" at offset 10 (creates sparse region)
		f.WriteAt([]byte("WORLD"), 10)

		data, _ := afs.FS.ReadFile(ctx, "/writeat.txt")
		if len(data) != 15 {
			t.Errorf("len = %d, want 15", len(data))
		}
		if string(data[:5]) != "HELLO" {
			t.Errorf("First 5 bytes = %q, want %q", data[:5], "HELLO")
		}
		if string(data[10:]) != "WORLD" {
			t.Errorf("Last 5 bytes = %q, want %q", data[10:], "WORLD")
		}
	})

	t.Run("WriteAt does not affect offset", func(t *testing.T) {
		_, f, _ := afs.FS.Create(ctx, "/writeat2.txt", 0o644)
		defer f.Close()

		initialOffset := f.Offset()

		f.WriteAt([]byte("test"), 100)

		if f.Offset() != initialOffset {
			t.Errorf("Offset changed from %d to %d", initialOffset, f.Offset())
		}
	})
}

func TestFile_IOCopy(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	content := []byte("Content to be copied using io.Copy")
	afs.FS.WriteFile(ctx, "/source.txt", content, 0o644)

	t.Run("io.Copy between files", func(t *testing.T) {
		src, _ := afs.FS.Open(ctx, "/source.txt", O_RDONLY)
		defer src.Close()

		_, dst, _ := afs.FS.Create(ctx, "/dest.txt", 0o644)
		defer dst.Close()

		n, err := io.Copy(dst, src)
		if err != nil {
			t.Fatalf("io.Copy failed: %v", err)
		}
		if n != int64(len(content)) {
			t.Errorf("Copied %d bytes, want %d", n, len(content))
		}

		// Verify destination
		data, _ := afs.FS.ReadFile(ctx, "/dest.txt")
		if string(data) != string(content) {
			t.Errorf("Dest content = %q, want %q", data, content)
		}
	})

	t.Run("io.Copy from bytes.Reader", func(t *testing.T) {
		src := bytes.NewReader([]byte("From bytes.Reader"))

		_, dst, _ := afs.FS.Create(ctx, "/from_reader.txt", 0o644)
		defer dst.Close()

		io.Copy(dst, src)

		data, _ := afs.FS.ReadFile(ctx, "/from_reader.txt")
		if string(data) != "From bytes.Reader" {
			t.Errorf("Content = %q", data)
		}
	})

	t.Run("io.Copy to bytes.Buffer", func(t *testing.T) {
		src, _ := afs.FS.Open(ctx, "/source.txt", O_RDONLY)
		defer src.Close()

		var buf bytes.Buffer
		io.Copy(&buf, src)

		if buf.String() != string(content) {
			t.Errorf("Buffer = %q, want %q", buf.String(), content)
		}
	})
}

func TestFile_LargeFileStreaming(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	// Create a file larger than one chunk (4KB)
	size := 50000
	content := make([]byte, size)
	for i := range content {
		content[i] = byte(i % 256)
	}

	t.Run("Stream write large file", func(t *testing.T) {
		_, f, _ := afs.FS.Create(ctx, "/large.bin", 0o644)
		defer f.Close()

		// Write in small chunks
		chunkSize := 1000
		for i := 0; i < size; i += chunkSize {
			end := i + chunkSize
			if end > size {
				end = size
			}
			n, err := f.Write(content[i:end])
			if err != nil {
				t.Fatalf("Write failed at offset %d: %v", i, err)
			}
			if n != end-i {
				t.Errorf("Wrote %d bytes, want %d", n, end-i)
			}
		}

		// Verify size
		fileSize, _ := f.Size()
		if fileSize != int64(size) {
			t.Errorf("Size = %d, want %d", fileSize, size)
		}
	})

	t.Run("Stream read large file", func(t *testing.T) {
		afs.FS.WriteFile(ctx, "/large2.bin", content, 0o644)

		f, _ := afs.FS.Open(ctx, "/large2.bin", O_RDONLY)
		defer f.Close()

		// Read in small chunks
		result := make([]byte, 0, size)
		buf := make([]byte, 1000)

		for {
			n, err := f.Read(buf)
			result = append(result, buf[:n]...)
			if err == io.EOF {
				break
			}
			if err != nil {
				t.Fatalf("Read failed: %v", err)
			}
		}

		if len(result) != size {
			t.Errorf("Read %d bytes, want %d", len(result), size)
		}

		// Verify content
		for i := range content {
			if result[i] != content[i] {
				t.Errorf("Mismatch at byte %d: got %d, want %d", i, result[i], content[i])
				break
			}
		}
	})
}

func TestFile_WithContext(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	afs.FS.WriteFile(ctx, "/ctx.txt", []byte("test"), 0o644)

	t.Run("WithContext creates copy", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/ctx.txt", O_RDONLY)
		defer f.Close()

		newCtx := context.WithValue(ctx, "key", "value")
		f2 := f.WithContext(newCtx)

		if f2 == f {
			t.Error("WithContext should return a new file")
		}

		// Should still work
		buf := make([]byte, 10)
		n, _ := f2.Read(buf)
		if string(buf[:n]) != "test" {
			t.Errorf("Read = %q, want %q", buf[:n], "test")
		}
	})

	t.Run("WithContext preserves offset", func(t *testing.T) {
		f, _ := afs.FS.Open(ctx, "/ctx.txt", O_RDONLY)
		defer f.Close()

		f.Seek(2, io.SeekStart)

		f2 := f.WithContext(ctx)
		if f2.Offset() != 2 {
			t.Errorf("Offset = %d, want 2", f2.Offset())
		}
	})
}

func TestFile_ReadWriteInterleaved(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	t.Run("Read then write", func(t *testing.T) {
		_, f, _ := afs.FS.Create(ctx, "/rw.txt", 0o644)
		defer f.Close()

		// Write initial content
		f.Write([]byte("HELLO"))

		// Seek back
		f.Seek(0, io.SeekStart)

		// Read
		buf := make([]byte, 5)
		f.Read(buf)
		if string(buf) != "HELLO" {
			t.Errorf("Read = %q, want HELLO", buf)
		}

		// Write more (appends)
		f.Write([]byte("WORLD"))

		// Check total content
		data, _ := afs.FS.ReadFile(ctx, "/rw.txt")
		if string(data) != "HELLOWORLD" {
			t.Errorf("Content = %q, want HELLOWORLD", data)
		}
	})

	t.Run("Overwrite in middle", func(t *testing.T) {
		afs.FS.WriteFile(ctx, "/overwrite.txt", []byte("AAAAABBBBBCCCCC"), 0o644)

		f, _ := afs.FS.Open(ctx, "/overwrite.txt", O_RDWR)
		defer f.Close()

		// Seek to middle
		f.Seek(5, io.SeekStart)

		// Overwrite
		f.Write([]byte("XXXXX"))

		data, _ := afs.FS.ReadFile(ctx, "/overwrite.txt")
		if string(data) != "AAAAAXXXXXCCCCC" {
			t.Errorf("Content = %q, want AAAAAXXXXXCCCCC", data)
		}
	})
}
