use crate::connection_pool::ConnectionPool;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use turso::Builder;

/// A key-value store backed by SQLite
#[derive(Clone)]
pub struct KvStore {
    pool: ConnectionPool,
}

impl KvStore {
    /// Create a new KV store
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let pool = ConnectionPool::new(db);
        let kv = Self { pool };
        kv.initialize().await?;
        Ok(kv)
    }

    /// Create a KV store from a connection pool
    pub async fn from_pool(pool: ConnectionPool) -> Result<Self> {
        let kv = Self { pool };
        kv.initialize().await?;
        Ok(kv)
    }

    /// Initialize the database schema
    async fn initialize(&self) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS kv_store (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                created_at INTEGER DEFAULT (unixepoch()),
                updated_at INTEGER DEFAULT (unixepoch())
            )",
            (),
        )
        .await?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_kv_store_created_at
            ON kv_store(created_at)",
            (),
        )
        .await?;

        Ok(())
    }

    /// Set a key-value pair
    pub async fn set<V: Serialize>(&self, key: &str, value: &V) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let serialized = serde_json::to_string(value)?;
        conn.execute(
            "INSERT INTO kv_store (key, value, updated_at)
            VALUES (?, ?, unixepoch())
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = unixepoch()",
            (key, serialized.as_str()),
        )
        .await?;
        Ok(())
    }

    /// Get a value by key
    pub async fn get<V: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<V>> {
        let conn = self.pool.get_conn().await?;
        let mut rows = conn
            .query("SELECT value FROM kv_store WHERE key = ?", (key,))
            .await?;

        if let Some(row) = rows.next().await? {
            if let Some(value_str) = row.get_value(0).ok().and_then(|v| {
                if let turso::Value::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            }) {
                let value: V = serde_json::from_str(&value_str)?;
                Ok(Some(value))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    /// Delete a key
    pub async fn delete(&self, key: &str) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        conn.execute("DELETE FROM kv_store WHERE key = ?", (key,))
            .await?;
        Ok(())
    }

    /// List all keys
    pub async fn keys(&self) -> Result<Vec<String>> {
        let conn = self.pool.get_conn().await?;
        let mut rows = conn.query("SELECT key FROM kv_store", ()).await?;
        let mut keys = Vec::new();
        while let Some(row) = rows.next().await? {
            if let Some(key) = row.get_value(0).ok().and_then(|v| {
                if let turso::Value::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            }) {
                keys.push(key);
            }
        }
        Ok(keys)
    }
}
