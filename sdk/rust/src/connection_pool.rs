//! Connection pool for turso database connections.
//!
//! This module provides a thread-safe connection pool that manages multiple
//! database connections. Each thread gets its own connection via `get_conn()`,
//! avoiding concurrent access issues with SQLite.

use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use turso::{Connection, Database};

/// Maximum number of concurrent connections allowed.
/// SQLite/turso MVCC requires single-writer to avoid stale snapshot errors.
/// Parallel FUSE requests are serialized at the database level.
const MAX_CONNECTIONS: usize = 1;

/// Database wrapper that supports both regular and sync databases.
enum DatabaseType {
    Local(Database),
    Sync(turso::sync::Database),
}

/// A pool of database connections.
///
/// The pool lazily creates connections as needed. Each call to `get_conn()`
/// returns a connection from the pool (or creates a new one if the pool is empty).
/// Connections are returned to the pool when dropped via `PooledConnection`.
///
/// Concurrency is limited to MAX_CONNECTIONS to avoid SQLite stale snapshot errors.
#[derive(Clone)]
pub struct ConnectionPool {
    inner: Arc<ConnectionPoolInner>,
}

struct ConnectionPoolInner {
    db: DatabaseType,
    pool: Mutex<Vec<Connection>>,
    /// Semaphore to limit concurrent connections
    semaphore: Arc<Semaphore>,
}

impl ConnectionPool {
    /// Create a new connection pool from a database.
    pub fn new(db: Database) -> Self {
        Self {
            inner: Arc::new(ConnectionPoolInner {
                db: DatabaseType::Local(db),
                pool: Mutex::new(Vec::new()),
                semaphore: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            }),
        }
    }

    /// Create a new connection pool from a sync database.
    pub fn new_sync(db: turso::sync::Database) -> Self {
        Self {
            inner: Arc::new(ConnectionPoolInner {
                db: DatabaseType::Sync(db),
                pool: Mutex::new(Vec::new()),
                semaphore: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            }),
        }
    }

    /// Get a connection from the pool.
    ///
    /// If the pool has available connections, one is returned.
    /// Otherwise, a new connection is created and configured with
    /// appropriate pragmas for concurrent access.
    ///
    /// This method will block if MAX_CONNECTIONS are already in use,
    /// waiting until one becomes available.
    ///
    /// The returned `PooledConnection` will return the connection to the pool
    /// when dropped.
    pub async fn get_conn(&self) -> anyhow::Result<PooledConnection> {
        // Acquire semaphore permit - blocks if max connections are in use
        let permit = self.inner.semaphore.clone().acquire_owned().await?;

        let conn = {
            let mut pool = self.inner.pool.lock().unwrap();
            pool.pop()
        };

        let conn = match conn {
            Some(c) => c,
            None => {
                // Create new connection
                let conn = match &self.inner.db {
                    DatabaseType::Local(db) => db.connect()?,
                    DatabaseType::Sync(db) => db.connect().await?,
                };
                // Set busy_timeout to handle concurrent access gracefully.
                // Without this, concurrent transactions fail immediately with SQLITE_BUSY.
                // This is per-connection setting, so must be set on each new connection.
                conn.execute("PRAGMA busy_timeout = 5000", ()).await?;
                // Disable synchronous mode for better performance with fsync() semantics.
                conn.execute("PRAGMA synchronous = OFF", ()).await?;
                conn
            }
        };

        Ok(PooledConnection {
            conn: Some(conn),
            pool: self.inner.clone(),
            _permit: permit,
        })
    }

    /// Get the underlying database reference (for creating additional connections).
    /// Returns None if this is a sync database.
    pub fn database(&self) -> Option<&Database> {
        match &self.inner.db {
            DatabaseType::Local(db) => Some(db),
            DatabaseType::Sync(_) => None,
        }
    }

    /// Get the underlying sync database reference.
    pub fn sync_database(&self) -> Option<&turso::sync::Database> {
        match &self.inner.db {
            DatabaseType::Local(_) => None,
            DatabaseType::Sync(db) => Some(db),
        }
    }
}

/// A connection borrowed from the pool.
///
/// When dropped, the connection is returned to the pool for reuse,
/// and the semaphore permit is released to allow another connection.
pub struct PooledConnection {
    conn: Option<Connection>,
    pool: Arc<ConnectionPoolInner>,
    /// Semaphore permit - released when this connection is dropped
    _permit: OwnedSemaphorePermit,
}

impl PooledConnection {
    /// Get a reference to the underlying connection.
    pub fn connection(&self) -> &Connection {
        self.conn.as_ref().expect("connection already taken")
    }
}

impl std::ops::Deref for PooledConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection()
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        // Don't return connections to the pool - prepared statement caching
        // causes "stale snapshot" errors when connections are reused after
        // other connections have modified the database.
        // Each operation gets a fresh connection.
        drop(self.conn.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso::Builder;

    #[tokio::test]
    async fn test_connection_pool_basic() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool = ConnectionPool::new(db);

        // Get a connection
        let conn = pool.get_conn().await.unwrap();
        assert!(conn.conn.is_some());

        // Drop it
        drop(conn);

        // Get another - should reuse the pooled one
        let conn2 = pool.get_conn().await.unwrap();
        assert!(conn2.conn.is_some());
    }

    #[tokio::test]
    async fn test_connection_pool_sequential() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool = ConnectionPool::new(db);

        // Get connections sequentially (release before getting next)
        for _ in 0..3 {
            let conn = pool.get_conn().await.unwrap();
            assert!(conn.conn.is_some());
            // conn dropped here, releasing the semaphore permit
        }

        // Pool should have 1 connection (reused each time)
        let pool_size = pool.inner.pool.lock().unwrap().len();
        assert_eq!(pool_size, 1);
    }
}
