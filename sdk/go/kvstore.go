package agentfs

import (
	"context"
	"database/sql"
	"encoding/json"
	"strings"

	_ "github.com/tursodatabase/go-libsql"
)

type KeyValuePair struct {
	Key   string
	Value any
}

type KvStore struct {
	db *sql.DB
}

func NewKvStore(db *sql.DB) *KvStore {
	return &KvStore{
		db: db,
	}
}

func NewKvStoreFromDatabase(db *sql.DB) *KvStore {
	kvStore := NewKvStore(db)
	kvStore.Initialize()
	return kvStore
}

func (kv *KvStore) Initialize() (err error) {
	ctx := context.Background()
	_, err = kv.db.ExecContext(ctx, `CREATE TABLE IF NOT EXISTS kv_store (
         key TEXT PRIMARY KEY,
         value TEXT NOT NULL,
         created_at INTEGER DEFAULT (unixepoch()),
         updated_at INTEGER DEFAULT (unixepoch())
       )`,
	)
	if err != nil {
		return
	}
	_, err = kv.db.ExecContext(ctx, `
       CREATE INDEX IF NOT EXISTS idx_kv_store_created_at
       ON kv_store(created_at)
    `)
	if err != nil {
		return
	}
	return
}

func (kv *KvStore) Set(key string, value any) (err error) {
	serialized, err := json.Marshal(value)
	if err != nil {
		return
	}
	serializedStr := string(serialized)
	ctx := context.Background()
	stmt, err := kv.db.PrepareContext(ctx, `
       INSERT INTO kv_store (key, value, updated_at)
       VALUES (?, ?, unixepoch())
       ON CONFLICT(key) DO UPDATE SET
         value = excluded.value,
         updated_at = unixepoch()
     `)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	_, err = stmt.ExecContext(ctx, key, serializedStr)
	if err != nil {
		return
	}
	return
}

func (kv *KvStore) Get(key string) (value any, err error) {
	ctx := context.Background()
	stmt, err := kv.db.PrepareContext(ctx, `SELECT value FROM kv_store WHERE key = ?`)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	row := stmt.QueryRowContext(ctx, key)
	var serializedStr string
	err = row.Scan(&serializedStr)
	if err != nil {
		return
	}
	err = json.Unmarshal([]byte(serializedStr), &value)
	return
}

func (kv *KvStore) Delete(key string) (err error) {
	ctx := context.Background()
	stmt, err := kv.db.PrepareContext(ctx, `DELETE FROM kv_store WHERE key = ?`)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	_, err = stmt.ExecContext(ctx, key)
	if err != nil {
		return
	}
	return
}

func (kv *KvStore) List(prefix string) (pairs []KeyValuePair, err error) {
	ctx := context.Background()
	stmt, err := kv.db.PrepareContext(ctx, `SELECT key, value FROM kv_store WHERE key LIKE ? ESCAPE '\\'`)
	if err != nil {
		return
	}
	defer func() { err = stmt.Close() }()
	prefix = strings.ReplaceAll(prefix, "\\", "\\\\")
	prefix = strings.ReplaceAll(prefix, "%", "\\%")
	prefix = strings.ReplaceAll(prefix, "_", "\\_")
	rows, err := stmt.QueryContext(ctx, prefix+"%")
	if err != nil {
		return
	}
	results := []KeyValuePair{}
	for rows.Next() {
		var result KeyValuePair
		var serializedValue string
		err = rows.Scan(&result.Key, &serializedValue)
		if err != nil {
			return
		}
		err = json.Unmarshal([]byte(serializedValue), &result.Value)
		if err != nil {
			return
		}
		results = append(results, result)
	}
	pairs = results
	return
}
