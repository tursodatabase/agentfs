//! Connection pool for Turso database connections.
//!
//! This module provides a thread-safe connection pool that manages database
//! connections with a maximum limit. When the pool is exhausted, callers block
//! until a connection becomes available or timeout occurs.

use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use turso::{Connection, Database};

use crate::error::{Error, Result};

/// Maximum number of connections in the pool.
const MAX_CONNECTIONS: usize = 2;

/// Default timeout for acquiring a connection from the pool.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// SQLite busy timeout - how long to wait when the database is locked.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Database wrapper that supports both regular and sync databases.
enum DatabaseType {
    Local(Database),
    Sync(turso::sync::Database),
}

/// A pool of database connections with a maximum limit.
///
/// The pool enforces a maximum number of concurrent connections. When all
/// connections are in use, `get_connection()` blocks until one becomes
/// available or the timeout expires (returning `ConnectionPoolTimeout`).
#[derive(Clone)]
pub struct ConnectionPool {
    inner: Arc<ConnectionPoolInner>,
}

struct ConnectionPoolInner {
    db: DatabaseType,
    /// Available connections ready to be reused
    pool: Mutex<Vec<Connection>>,
    /// Semaphore to limit concurrent connections
    semaphore: Arc<Semaphore>,
    /// Timeout for acquiring a connection
    timeout: Duration,
}

impl ConnectionPool {
    /// Create a new connection pool from a database.
    pub fn new(db: Database) -> Self {
        Self::with_timeout(DatabaseType::Local(db), DEFAULT_TIMEOUT)
    }

    /// Create a new connection pool from a sync database.
    pub fn new_sync(db: turso::sync::Database) -> Self {
        Self::with_timeout(DatabaseType::Sync(db), DEFAULT_TIMEOUT)
    }

    /// Create a connection pool with a custom timeout.
    fn with_timeout(db: DatabaseType, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(ConnectionPoolInner {
                db,
                pool: Mutex::new(Vec::new()),
                semaphore: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
                timeout,
            }),
        }
    }

    /// Get a connection from the pool.
    ///
    /// If a pooled connection is available, it is returned immediately.
    /// Otherwise, if the pool hasn't reached max capacity, a new connection
    /// is created. If at max capacity, this blocks until a connection is
    /// returned to the pool or timeout expires.
    ///
    /// # Errors
    ///
    /// Returns `Error::ConnectionPoolTimeout` if no connection becomes
    /// available within the timeout period.
    pub async fn get_connection(&self) -> Result<PooledConnection> {
        // Try to acquire a permit with timeout
        let permit = tokio::time::timeout(
            self.inner.timeout,
            Arc::clone(&self.inner.semaphore).acquire_owned(),
        )
        .await
        .map_err(|_| Error::ConnectionPoolTimeout)?
        .map_err(|_| Error::Internal("semaphore closed".to_string()))?;

        // We have a permit - try to get an existing connection or create new one
        let conn = {
            let mut pool = self.inner.pool.lock().await;
            pool.pop()
        };

        let conn = match conn {
            Some(c) => c,
            None => {
                let conn = match &self.inner.db {
                    DatabaseType::Local(db) => db.connect()?,
                    DatabaseType::Sync(db) => db.connect().await?,
                };
                conn.busy_timeout(BUSY_TIMEOUT)?;
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
/// When dropped, the connection is returned to the pool for reuse and the
/// semaphore permit is released, allowing another caller to acquire a connection.
pub struct PooledConnection {
    conn: Option<Connection>,
    pool: Arc<ConnectionPoolInner>,
    /// Held permit - released when this is dropped
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
        if let Some(conn) = self.conn.take() {
            // Return connection to pool - use try_lock to avoid blocking in drop
            // If we can't get the lock, just drop the connection (it will be recreated)
            if let Ok(mut pool) = self.pool.pool.try_lock() {
                pool.push(conn);
            }
            // Permit is automatically released when _permit is dropped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use turso::Builder;

    #[tokio::test]
    async fn test_connection_pool_basic() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool = ConnectionPool::new(db);

        // Get a connection
        let conn = pool.get_connection().await.unwrap();
        assert!(conn.conn.is_some());

        // Drop it
        drop(conn);

        // Get another - should reuse the pooled one
        let conn2 = pool.get_connection().await.unwrap();
        assert!(conn2.conn.is_some());
    }

    #[tokio::test]
    async fn test_connection_pool_max_one() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool = ConnectionPool::new(db);

        // Get the one allowed connection
        let conn1 = pool.get_connection().await.unwrap();
        assert!(conn1.conn.is_some());

        // Try to get another - should timeout quickly
        let pool_clone = pool.clone();
        let result =
            tokio::time::timeout(Duration::from_millis(100), pool_clone.get_connection()).await;

        // Should timeout since we only have 1 connection allowed
        assert!(result.is_err());

        // Drop conn1, now we should be able to get a connection
        drop(conn1);
        let conn2 = pool.get_connection().await.unwrap();
        assert!(conn2.conn.is_some());
    }

    #[tokio::test]
    async fn test_connection_pool_timeout_error() {
        // Create pool with very short timeout
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool = ConnectionPool::with_timeout(DatabaseType::Local(db), Duration::from_millis(50));

        // Hold the one connection
        let _conn1 = pool.get_connection().await.unwrap();

        // Try to get another - should return ConnectionPoolTimeout
        let result = pool.get_connection().await;
        assert!(matches!(result, Err(Error::ConnectionPoolTimeout)));
    }

    #[tokio::test]
    async fn test_connection_pool_concurrent_waiters() {
        let db = Builder::new_local(":memory:").build().await.unwrap();
        let pool = ConnectionPool::new(db);
        let counter = Arc::new(AtomicUsize::new(0));

        // Spawn multiple tasks that all want the connection
        let mut handles = vec![];
        for _ in 0..5 {
            let pool = pool.clone();
            let counter = counter.clone();
            handles.push(tokio::spawn(async move {
                let _conn = pool.get_connection().await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                // Hold connection briefly
                tokio::time::sleep(Duration::from_millis(10)).await;
            }));
        }

        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // All 5 should have completed (serially, since max=1)
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }
}
