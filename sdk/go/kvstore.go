package agentfs

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"strings"
)

// KVStore provides key-value storage backed by SQLite.
type KVStore struct {
	db *sql.DB
}

// Set stores a value (JSON-serialized) for the given key.
func (kv *KVStore) Set(ctx context.Context, key string, value any) error {
	jsonValue, err := json.Marshal(value)
	if err != nil {
		return fmt.Errorf("failed to marshal value: %w", err)
	}

	if _, err := kv.db.ExecContext(ctx, kvSet, key, string(jsonValue)); err != nil {
		return fmt.Errorf("failed to set key: %w", err)
	}

	return nil
}

// Get retrieves a value and unmarshals it into dest.
// Returns an error if the key does not exist.
func (kv *KVStore) Get(ctx context.Context, key string, dest any) error {
	var jsonValue string
	err := kv.db.QueryRowContext(ctx, kvGet, key).Scan(&jsonValue)
	if err == sql.ErrNoRows {
		return fmt.Errorf("key not found: %s", key)
	}
	if err != nil {
		return fmt.Errorf("failed to get key: %w", err)
	}

	if err := json.Unmarshal([]byte(jsonValue), dest); err != nil {
		return fmt.Errorf("failed to unmarshal value: %w", err)
	}

	return nil
}

// GetRaw retrieves the raw JSON value for a key.
// Returns an error if the key does not exist.
func (kv *KVStore) GetRaw(ctx context.Context, key string) (json.RawMessage, error) {
	var jsonValue string
	err := kv.db.QueryRowContext(ctx, kvGet, key).Scan(&jsonValue)
	if err == sql.ErrNoRows {
		return nil, fmt.Errorf("key not found: %s", key)
	}
	if err != nil {
		return nil, fmt.Errorf("failed to get key: %w", err)
	}

	return json.RawMessage(jsonValue), nil
}

// Delete removes a key.
func (kv *KVStore) Delete(ctx context.Context, key string) error {
	if _, err := kv.db.ExecContext(ctx, kvDelete, key); err != nil {
		return fmt.Errorf("failed to delete key: %w", err)
	}
	return nil
}

// Has checks if a key exists.
func (kv *KVStore) Has(ctx context.Context, key string) (bool, error) {
	var exists int
	err := kv.db.QueryRowContext(ctx, kvHas, key).Scan(&exists)
	if err == sql.ErrNoRows {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("failed to check key: %w", err)
	}
	return exists == 1, nil
}

// Keys returns all keys, optionally filtered by prefix.
// If prefix is empty, all keys are returned.
func (kv *KVStore) Keys(ctx context.Context, prefix string) ([]string, error) {
	var rows *sql.Rows
	var err error

	if prefix == "" {
		rows, err = kv.db.QueryContext(ctx, kvKeys)
	} else {
		pattern := escapePattern(prefix) + "%"
		rows, err = kv.db.QueryContext(ctx, kvKeysWithPrefix, pattern)
	}

	if err != nil {
		return nil, fmt.Errorf("failed to list keys: %w", err)
	}
	defer rows.Close()

	var keys []string
	for rows.Next() {
		var key string
		if err := rows.Scan(&key); err != nil {
			return nil, err
		}
		keys = append(keys, key)
	}

	return keys, rows.Err()
}

// List returns all key entries with metadata, optionally filtered by prefix.
func (kv *KVStore) List(ctx context.Context, prefix string) ([]KVEntry, error) {
	var rows *sql.Rows
	var err error

	if prefix == "" {
		rows, err = kv.db.QueryContext(ctx, kvList)
	} else {
		pattern := escapePattern(prefix) + "%"
		rows, err = kv.db.QueryContext(ctx, kvListWithPrefix, pattern)
	}

	if err != nil {
		return nil, fmt.Errorf("failed to list entries: %w", err)
	}
	defer rows.Close()

	var entries []KVEntry
	for rows.Next() {
		var entry KVEntry
		if err := rows.Scan(&entry.Key, &entry.CreatedAt, &entry.UpdatedAt); err != nil {
			return nil, err
		}
		entries = append(entries, entry)
	}

	return entries, rows.Err()
}

// Clear removes all keys, optionally filtered by prefix.
func (kv *KVStore) Clear(ctx context.Context, prefix string) error {
	var err error

	if prefix == "" {
		_, err = kv.db.ExecContext(ctx, kvClear)
	} else {
		pattern := escapePattern(prefix) + "%"
		_, err = kv.db.ExecContext(ctx, kvClearWithPrefix, pattern)
	}

	if err != nil {
		return fmt.Errorf("failed to clear keys: %w", err)
	}
	return nil
}

// escapePattern escapes special characters for LIKE pattern matching
func escapePattern(s string) string {
	s = strings.ReplaceAll(s, "\\", "\\\\")
	s = strings.ReplaceAll(s, "%", "\\%")
	s = strings.ReplaceAll(s, "_", "\\_")
	return s
}
