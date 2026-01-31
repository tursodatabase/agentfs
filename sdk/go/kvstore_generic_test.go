package agentfs

import (
	"context"
	"testing"
)

func TestKVGet_Generic(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	t.Run("Get string", func(t *testing.T) {
		afs.KV.Set(ctx, "str", "hello")

		val, err := KVGet[string](ctx, afs.KV, "str")
		if err != nil {
			t.Fatalf("KVGet failed: %v", err)
		}
		if val != "hello" {
			t.Errorf("val = %q, want %q", val, "hello")
		}
	})

	t.Run("Get int", func(t *testing.T) {
		afs.KV.Set(ctx, "num", 42)

		val, err := KVGet[int](ctx, afs.KV, "num")
		if err != nil {
			t.Fatalf("KVGet failed: %v", err)
		}
		if val != 42 {
			t.Errorf("val = %d, want %d", val, 42)
		}
	})

	t.Run("Get float", func(t *testing.T) {
		afs.KV.Set(ctx, "float", 3.14)

		val, err := KVGet[float64](ctx, afs.KV, "float")
		if err != nil {
			t.Fatalf("KVGet failed: %v", err)
		}
		if val != 3.14 {
			t.Errorf("val = %f, want %f", val, 3.14)
		}
	})

	t.Run("Get bool", func(t *testing.T) {
		afs.KV.Set(ctx, "flag", true)

		val, err := KVGet[bool](ctx, afs.KV, "flag")
		if err != nil {
			t.Fatalf("KVGet failed: %v", err)
		}
		if val != true {
			t.Errorf("val = %v, want %v", val, true)
		}
	})

	t.Run("Get struct", func(t *testing.T) {
		type Config struct {
			Name    string `json:"name"`
			Version int    `json:"version"`
			Debug   bool   `json:"debug"`
		}

		input := Config{Name: "myapp", Version: 2, Debug: true}
		afs.KV.Set(ctx, "config", input)

		val, err := KVGet[Config](ctx, afs.KV, "config")
		if err != nil {
			t.Fatalf("KVGet failed: %v", err)
		}
		if val != input {
			t.Errorf("val = %+v, want %+v", val, input)
		}
	})

	t.Run("Get slice", func(t *testing.T) {
		input := []string{"a", "b", "c"}
		afs.KV.Set(ctx, "slice", input)

		val, err := KVGet[[]string](ctx, afs.KV, "slice")
		if err != nil {
			t.Fatalf("KVGet failed: %v", err)
		}
		if len(val) != 3 || val[0] != "a" || val[1] != "b" || val[2] != "c" {
			t.Errorf("val = %v, want %v", val, input)
		}
	})

	t.Run("Get map", func(t *testing.T) {
		input := map[string]int{"a": 1, "b": 2}
		afs.KV.Set(ctx, "map", input)

		val, err := KVGet[map[string]int](ctx, afs.KV, "map")
		if err != nil {
			t.Fatalf("KVGet failed: %v", err)
		}
		if val["a"] != 1 || val["b"] != 2 {
			t.Errorf("val = %v, want %v", val, input)
		}
	})

	t.Run("Get non-existent key", func(t *testing.T) {
		_, err := KVGet[string](ctx, afs.KV, "nonexistent")
		if err == nil {
			t.Error("Expected error for non-existent key")
		}
	})

	t.Run("Get with type mismatch", func(t *testing.T) {
		afs.KV.Set(ctx, "string_val", "not a number")

		// Try to get string as int - should fail during unmarshal
		_, err := KVGet[int](ctx, afs.KV, "string_val")
		if err == nil {
			t.Error("Expected error for type mismatch")
		}
	})
}

func TestKVGetOrDefault_Generic(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	t.Run("Returns value when exists", func(t *testing.T) {
		afs.KV.Set(ctx, "exists", "found")

		val, err := KVGetOrDefault(ctx, afs.KV, "exists", "default")
		if err != nil {
			t.Fatalf("KVGetOrDefault failed: %v", err)
		}
		if val != "found" {
			t.Errorf("val = %q, want %q", val, "found")
		}
	})

	t.Run("Returns default when not exists", func(t *testing.T) {
		val, err := KVGetOrDefault(ctx, afs.KV, "not_exists", "default_value")
		if err != nil {
			t.Fatalf("KVGetOrDefault failed: %v", err)
		}
		if val != "default_value" {
			t.Errorf("val = %q, want %q", val, "default_value")
		}
	})

	t.Run("Returns default int", func(t *testing.T) {
		val, err := KVGetOrDefault(ctx, afs.KV, "missing_int", 100)
		if err != nil {
			t.Fatalf("KVGetOrDefault failed: %v", err)
		}
		if val != 100 {
			t.Errorf("val = %d, want %d", val, 100)
		}
	})

	t.Run("Returns default bool", func(t *testing.T) {
		val, err := KVGetOrDefault(ctx, afs.KV, "missing_bool", true)
		if err != nil {
			t.Fatalf("KVGetOrDefault failed: %v", err)
		}
		if val != true {
			t.Errorf("val = %v, want %v", val, true)
		}
	})

	t.Run("Returns default struct", func(t *testing.T) {
		type Settings struct {
			Theme string `json:"theme"`
		}

		defaultSettings := Settings{Theme: "dark"}
		val, err := KVGetOrDefault(ctx, afs.KV, "missing_settings", defaultSettings)
		if err != nil {
			t.Fatalf("KVGetOrDefault failed: %v", err)
		}
		if val != defaultSettings {
			t.Errorf("val = %+v, want %+v", val, defaultSettings)
		}
	})

	t.Run("Returns error on type mismatch", func(t *testing.T) {
		afs.KV.Set(ctx, "string_for_int", "not an int")

		_, err := KVGetOrDefault(ctx, afs.KV, "string_for_int", 0)
		if err == nil {
			t.Error("Expected error for type mismatch")
		}
	})
}

func TestKVGetOrZero_Generic(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	t.Run("Returns value when exists", func(t *testing.T) {
		afs.KV.Set(ctx, "has_value", 42)

		val, err := KVGetOrZero[int](ctx, afs.KV, "has_value")
		if err != nil {
			t.Fatalf("KVGetOrZero failed: %v", err)
		}
		if val != 42 {
			t.Errorf("val = %d, want %d", val, 42)
		}
	})

	t.Run("Returns zero int", func(t *testing.T) {
		val, err := KVGetOrZero[int](ctx, afs.KV, "missing")
		if err != nil {
			t.Fatalf("KVGetOrZero failed: %v", err)
		}
		if val != 0 {
			t.Errorf("val = %d, want %d", val, 0)
		}
	})

	t.Run("Returns zero string", func(t *testing.T) {
		val, err := KVGetOrZero[string](ctx, afs.KV, "missing")
		if err != nil {
			t.Fatalf("KVGetOrZero failed: %v", err)
		}
		if val != "" {
			t.Errorf("val = %q, want %q", val, "")
		}
	})

	t.Run("Returns zero bool", func(t *testing.T) {
		val, err := KVGetOrZero[bool](ctx, afs.KV, "missing")
		if err != nil {
			t.Fatalf("KVGetOrZero failed: %v", err)
		}
		if val != false {
			t.Errorf("val = %v, want %v", val, false)
		}
	})

	t.Run("Returns nil slice", func(t *testing.T) {
		val, err := KVGetOrZero[[]string](ctx, afs.KV, "missing")
		if err != nil {
			t.Fatalf("KVGetOrZero failed: %v", err)
		}
		if val != nil {
			t.Errorf("val = %v, want nil", val)
		}
	})
}

func TestKVSet_Generic(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	t.Run("Set and get string", func(t *testing.T) {
		err := KVSet(ctx, afs.KV, "gen_str", "generic string")
		if err != nil {
			t.Fatalf("KVSet failed: %v", err)
		}

		val, _ := KVGet[string](ctx, afs.KV, "gen_str")
		if val != "generic string" {
			t.Errorf("val = %q, want %q", val, "generic string")
		}
	})

	t.Run("Set and get struct", func(t *testing.T) {
		type User struct {
			ID   int    `json:"id"`
			Name string `json:"name"`
		}

		user := User{ID: 1, Name: "Alice"}
		err := KVSet(ctx, afs.KV, "user", user)
		if err != nil {
			t.Fatalf("KVSet failed: %v", err)
		}

		val, _ := KVGet[User](ctx, afs.KV, "user")
		if val != user {
			t.Errorf("val = %+v, want %+v", val, user)
		}
	})
}

func TestKVGeneric_RealWorldExample(t *testing.T) {
	ctx := context.Background()
	afs := setupTestDB(t)
	defer afs.Close()

	// Simulate a real-world usage pattern

	type AppConfig struct {
		Debug       bool     `json:"debug"`
		LogLevel    string   `json:"log_level"`
		MaxRetries  int      `json:"max_retries"`
		AllowedTags []string `json:"allowed_tags"`
	}

	t.Run("Application config workflow", func(t *testing.T) {
		// Set default config
		defaultConfig := AppConfig{
			Debug:       false,
			LogLevel:    "info",
			MaxRetries:  3,
			AllowedTags: []string{"production"},
		}

		// Try to get config, use default if not exists
		config, err := KVGetOrDefault(ctx, afs.KV, "app:config", defaultConfig)
		if err != nil {
			t.Fatalf("Failed to get config: %v", err)
		}

		// Should be the default
		if config.LogLevel != "info" {
			t.Errorf("LogLevel = %q, want %q", config.LogLevel, "info")
		}

		// Update config
		config.Debug = true
		config.LogLevel = "debug"
		err = KVSet(ctx, afs.KV, "app:config", config)
		if err != nil {
			t.Fatalf("Failed to set config: %v", err)
		}

		// Retrieve updated config
		updated, err := KVGet[AppConfig](ctx, afs.KV, "app:config")
		if err != nil {
			t.Fatalf("Failed to get updated config: %v", err)
		}

		if !updated.Debug {
			t.Error("Debug should be true")
		}
		if updated.LogLevel != "debug" {
			t.Errorf("LogLevel = %q, want %q", updated.LogLevel, "debug")
		}
	})

	t.Run("Counter pattern", func(t *testing.T) {
		// Get current count (default 0)
		count, _ := KVGetOrZero[int](ctx, afs.KV, "stats:requests")

		// Increment
		count++
		KVSet(ctx, afs.KV, "stats:requests", count)

		// Verify
		newCount, _ := KVGet[int](ctx, afs.KV, "stats:requests")
		if newCount != 1 {
			t.Errorf("count = %d, want 1", newCount)
		}
	})
}
