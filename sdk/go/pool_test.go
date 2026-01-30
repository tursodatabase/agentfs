package agentfs

import (
	"context"
	"testing"
	"time"
)

func TestPoolOptions_MaxOpenConns(t *testing.T) {
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		Pool: PoolOptions{
			MaxOpenConns: 1,
		},
	})
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	defer afs.Close()

	// Verify database works with pool settings
	err = afs.FS.WriteFile(ctx, "/test.txt", []byte("hello"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}

	data, err := afs.FS.ReadFile(ctx, "/test.txt")
	if err != nil {
		t.Fatalf("ReadFile failed: %v", err)
	}
	if string(data) != "hello" {
		t.Errorf("Expected 'hello', got %q", data)
	}
}

func TestPoolOptions_MaxIdleConns(t *testing.T) {
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		Pool: PoolOptions{
			MaxOpenConns: 5,
			MaxIdleConns: 5,
		},
	})
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	defer afs.Close()

	// Verify database works with pool settings
	err = afs.FS.WriteFile(ctx, "/test.txt", []byte("hello"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}
}

func TestPoolOptions_ConnMaxLifetime(t *testing.T) {
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		Pool: PoolOptions{
			ConnMaxLifetime: 1 * time.Hour,
		},
	})
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	defer afs.Close()

	// Verify database works with pool settings
	err = afs.FS.WriteFile(ctx, "/test.txt", []byte("hello"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}
}

func TestPoolOptions_ConnMaxIdleTime(t *testing.T) {
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		Pool: PoolOptions{
			ConnMaxIdleTime: 5 * time.Minute,
		},
	})
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	defer afs.Close()

	// Verify database works with pool settings
	err = afs.FS.WriteFile(ctx, "/test.txt", []byte("hello"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}
}

func TestPoolOptions_AllSettings(t *testing.T) {
	ctx := context.Background()
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		Pool: PoolOptions{
			MaxOpenConns:    2,
			MaxIdleConns:    2,
			ConnMaxLifetime: 30 * time.Minute,
			ConnMaxIdleTime: 5 * time.Minute,
		},
	})
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	defer afs.Close()

	// Verify database works with all pool settings
	for i := 0; i < 10; i++ {
		err = afs.KV.Set(ctx, "key", i)
		if err != nil {
			t.Fatalf("KV.Set failed: %v", err)
		}

		var val int
		err = afs.KV.Get(ctx, "key", &val)
		if err != nil {
			t.Fatalf("KV.Get failed: %v", err)
		}
		if val != i {
			t.Errorf("Expected %d, got %d", i, val)
		}
	}
}

func TestPoolOptions_DefaultsWhenZero(t *testing.T) {
	ctx := context.Background()

	// All zero values - should use database/sql defaults
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
		Pool: PoolOptions{}, // All zero
	})
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	defer afs.Close()

	// Verify database works with default settings
	err = afs.FS.WriteFile(ctx, "/test.txt", []byte("hello"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}
}

func TestPoolOptions_NotSpecified(t *testing.T) {
	ctx := context.Background()

	// Pool not specified at all
	afs, err := Open(ctx, AgentFSOptions{
		Path: ":memory:",
	})
	if err != nil {
		t.Fatalf("Open failed: %v", err)
	}
	defer afs.Close()

	// Verify database works without pool settings
	err = afs.FS.WriteFile(ctx, "/test.txt", []byte("hello"), 0o644)
	if err != nil {
		t.Fatalf("WriteFile failed: %v", err)
	}
}
