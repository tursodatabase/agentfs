//! Connection pool for concurrent database access.
//!
//! The turso database connection doesn't support concurrent transactions on a single
//! connection. This module provides a pool of connections that can be borrowed for
//! individual operations, enabling concurrent filesystem access.

use anyhow::Result;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use turso::{Builder, Connection, Database};

/// A pool of database connections for concurrent access.
///
/// Each connection can only be used by one task at a time. When a task needs
/// database access, it borrows a connection from the pool. When done, the
/// connection is returned to the pool for reuse.
///
/// If all connections are in use, `get()` will asynchronously wait until
/// a connection becomes available.
pub struct ConnectionPool {
    /// The database instance used to create new connections
    db: Database,
    /// Available connections ready to be borrowed (Arc-wrapped for sharing with PooledConnection)
    available: Arc<Mutex<Vec<Arc<Connection>>>>,
    /// Semaphore to limit concurrent connections
    semaphore: Arc<Semaphore>,
    /// Maximum number of connections
    max_connections: usize,
}

impl ConnectionPool {
    /// Create a new connection pool for the given database path.
    ///
    /// # Arguments
    /// * `db_path` - Path to the SQLite database file
    /// * `max_connections` - Maximum number of concurrent connections
    pub async fn new(db_path: &str, max_connections: usize) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;

        Ok(Self {
            db,
            available: Arc::new(Mutex::new(Vec::with_capacity(max_connections))),
            semaphore: Arc::new(Semaphore::new(max_connections)),
            max_connections,
        })
    }

    /// Get a connection from the pool.
    ///
    /// If a connection is available in the pool, it is returned immediately.
    /// If all connections are in use, this method will asynchronously wait
    /// until one becomes available.
    ///
    /// The returned `PooledConnection` will automatically return the connection
    /// to the pool when dropped.
    pub async fn get(&self) -> Result<PooledConnection> {
        // Acquire a permit - this will wait if all connections are in use
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("Semaphore closed: {}", e))?;

        // Try to get an existing connection from the pool
        let conn = {
            let mut available = self.available.lock().await;
            available.pop()
        };

        let conn = match conn {
            Some(conn) => conn,
            None => {
                // No available connection, create a new one
                Arc::new(self.db.connect()?)
            }
        };

        Ok(PooledConnection {
            conn: Some(conn),
            available: self.available.clone(),
            _permit: permit,
            max_connections: self.max_connections,
        })
    }

    /// Get the number of available connections in the pool.
    pub async fn available_count(&self) -> usize {
        self.available.lock().await.len()
    }

    /// Get the number of permits available (connections not in use).
    pub fn permits_available(&self) -> usize {
        self.semaphore.available_permits()
    }
}

/// A connection borrowed from the pool.
///
/// When dropped, the connection is automatically returned to the pool
/// and the semaphore permit is released.
pub struct PooledConnection {
    conn: Option<Arc<Connection>>,
    available: Arc<Mutex<Vec<Arc<Connection>>>>,
    _permit: OwnedSemaphorePermit,
    max_connections: usize,
}

impl Deref for PooledConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().unwrap()
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Return connection to pool synchronously
            // We use try_lock to avoid blocking in drop
            if let Ok(mut available) = self.available.try_lock() {
                if available.len() < self.max_connections {
                    available.push(conn);
                }
            }
            // If we can't get the lock or pool is full, connection is dropped
            // The semaphore permit is released automatically when _permit is dropped
        }
    }
}

impl PooledConnection {
    /// Get a reference to the underlying connection.
    pub fn connection(&self) -> &Connection {
        self.conn.as_ref().unwrap()
    }
}
