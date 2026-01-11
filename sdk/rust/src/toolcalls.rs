use crate::connection_pool::ConnectionPool;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};
use turso::{Builder, Value};

/// Status of a tool call
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallStatus {
    Pending,
    Success,
    Error,
}

impl fmt::Display for ToolCallStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolCallStatus::Pending => write!(f, "pending"),
            ToolCallStatus::Success => write!(f, "success"),
            ToolCallStatus::Error => write!(f, "error"),
        }
    }
}

impl From<&str> for ToolCallStatus {
    fn from(s: &str) -> Self {
        match s {
            "success" => ToolCallStatus::Success,
            "error" => ToolCallStatus::Error,
            _ => ToolCallStatus::Pending,
        }
    }
}

/// A tool call record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: i64,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: ToolCallStatus,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

/// Statistics for a specific tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallStats {
    pub name: String,
    pub total_calls: i64,
    pub successful: i64,
    pub failed: i64,
    pub avg_duration_ms: f64,
}

/// Tool calls tracker backed by SQLite
#[derive(Clone)]
pub struct ToolCalls {
    pool: ConnectionPool,
}

impl ToolCalls {
    /// Create a new tool calls tracker
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let pool = ConnectionPool::new(db);
        let tc = Self { pool };
        tc.initialize().await?;
        Ok(tc)
    }

    /// Create a tool calls tracker from a connection pool
    pub async fn from_pool(pool: ConnectionPool) -> Result<Self> {
        let tc = Self { pool };
        tc.initialize().await?;
        Ok(tc)
    }

    /// Initialize the database schema
    async fn initialize(&self) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS tool_calls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                parameters TEXT,
                result TEXT,
                error TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                duration_ms INTEGER
            )",
            (),
        )
        .await?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tool_calls_name
            ON tool_calls(name)",
            (),
        )
        .await?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tool_calls_started_at
            ON tool_calls(started_at)",
            (),
        )
        .await?;

        Ok(())
    }

    /// Start a new tool call and mark it as pending
    /// Returns the ID of the created tool call record
    pub async fn start(&self, name: &str, parameters: Option<serde_json::Value>) -> Result<i64> {
        let conn = self.pool.get_conn().await?;
        let serialized_params = parameters.map(|p| serde_json::to_string(&p)).transpose()?;
        let started_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        let mut stmt = conn
            .prepare(
                "INSERT INTO tool_calls (name, parameters, status, started_at)
                VALUES (?, ?, 'pending', ?) RETURNING id",
            )
            .await?;
        let row = stmt
            .query_row((name, serialized_params.as_deref().unwrap_or(""), started_at))
            .await?;

        let id = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .ok_or_else(|| anyhow::anyhow!("Failed to get tool call ID"))?;
        Ok(id)
    }

    /// Mark a tool call as successful
    pub async fn success(&self, id: i64, result: Option<serde_json::Value>) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let serialized_result = result.map(|r| serde_json::to_string(&r)).transpose()?;
        let completed_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        // Get the started_at time to calculate duration
        let mut rows = conn
            .query("SELECT started_at FROM tool_calls WHERE id = ?", (id,))
            .await?;

        let started_at = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| anyhow::anyhow!("Invalid started_at value"))?
        } else {
            anyhow::bail!("Tool call not found");
        };

        let duration_ms = (completed_at - started_at) * 1000;

        conn.execute(
            "UPDATE tool_calls
            SET result = ?, status = 'success', completed_at = ?, duration_ms = ?
            WHERE id = ?",
            (
                serialized_result.as_deref().unwrap_or(""),
                completed_at,
                duration_ms,
                id,
            ),
        )
        .await?;

        Ok(())
    }

    /// Record a completed tool call (spec-compliant insert-only method)
    /// Either result or error should be provided, not both
    /// Returns the ID of the created tool call record
    pub async fn record(
        &self,
        name: &str,
        started_at: i64,
        completed_at: i64,
        parameters: Option<serde_json::Value>,
        result: Option<serde_json::Value>,
        error: Option<&str>,
    ) -> Result<i64> {
        let conn = self.pool.get_conn().await?;
        let serialized_params = parameters.map(|p| serde_json::to_string(&p)).transpose()?;
        let serialized_result = result.map(|r| serde_json::to_string(&r)).transpose()?;
        let duration_ms = (completed_at - started_at) * 1000;
        let status = if error.is_some() { "error" } else { "success" };

        let mut stmt = conn
            .prepare(
                "INSERT INTO tool_calls (name, parameters, result, error, status, started_at, completed_at, duration_ms)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id"
            )
            .await?;

        let row = stmt
            .query_row((
                name,
                serialized_params.as_deref().unwrap_or(""),
                serialized_result.as_deref().unwrap_or(""),
                error.unwrap_or(""),
                status,
                started_at,
                completed_at,
                duration_ms,
            ))
            .await?;
        let id = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .ok_or_else(|| anyhow::anyhow!("Failed to get tool call ID"))?;
        Ok(id)
    }

    /// Mark a tool call as failed
    pub async fn error(&self, id: i64, error: &str) -> Result<()> {
        let conn = self.pool.get_conn().await?;
        let completed_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        // Get the started_at time to calculate duration
        let mut rows = conn
            .query("SELECT started_at FROM tool_calls WHERE id = ?", (id,))
            .await?;

        let started_at = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| anyhow::anyhow!("Invalid started_at value"))?
        } else {
            anyhow::bail!("Tool call not found");
        };

        let duration_ms = (completed_at - started_at) * 1000;

        conn.execute(
            "UPDATE tool_calls
            SET error = ?, status = 'error', completed_at = ?, duration_ms = ?
            WHERE id = ?",
            (error, completed_at, duration_ms, id),
        )
        .await?;

        Ok(())
    }

    /// Get a tool call by ID
    pub async fn get(&self, id: i64) -> Result<Option<ToolCall>> {
        let conn = self.pool.get_conn().await?;
        let mut rows = conn
            .query(
                "SELECT id, name, parameters, result, error, status, started_at, completed_at, duration_ms
                FROM tool_calls WHERE id = ?",
                (id,),
            )
            .await?;

        if let Some(row) = rows.next().await? {
            Ok(Some(Self::row_to_tool_call(&row)?))
        } else {
            Ok(None)
        }
    }

    /// Get recent tool calls with optional limit
    pub async fn recent(&self, limit: Option<i64>) -> Result<Vec<ToolCall>> {
        let conn = self.pool.get_conn().await?;
        let limit = limit.unwrap_or(100);
        let mut rows = conn
            .query(
                "SELECT id, name, parameters, result, error, status, started_at, completed_at, duration_ms
                FROM tool_calls
                ORDER BY started_at DESC
                LIMIT ?",
                (limit,),
            )
            .await?;

        let mut calls = Vec::new();
        while let Some(row) = rows.next().await? {
            calls.push(Self::row_to_tool_call(&row)?);
        }

        Ok(calls)
    }

    /// Get statistics for a specific tool
    pub async fn stats_for(&self, name: &str) -> Result<Option<ToolCallStats>> {
        let conn = self.pool.get_conn().await?;
        let mut rows = conn
            .query(
                "SELECT
                    name,
                    COUNT(*) as total_calls,
                    SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END) as successful,
                    SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) as failed,
                    AVG(CASE WHEN duration_ms IS NOT NULL THEN duration_ms ELSE 0 END) as avg_duration_ms
                FROM tool_calls
                WHERE name = ?
                GROUP BY name",
                (name,),
            )
            .await?;

        if let Some(row) = rows.next().await? {
            Ok(Some(Self::row_to_stats(&row)?))
        } else {
            Ok(None)
        }
    }

    /// Get statistics for all tools
    pub async fn stats(&self) -> Result<Vec<ToolCallStats>> {
        let conn = self.pool.get_conn().await?;
        let mut rows = conn
            .query(
                "SELECT
                    name,
                    COUNT(*) as total_calls,
                    SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END) as successful,
                    SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) as failed,
                    AVG(CASE WHEN duration_ms IS NOT NULL THEN duration_ms ELSE 0 END) as avg_duration_ms
                FROM tool_calls
                GROUP BY name
                ORDER BY total_calls DESC",
                (),
            )
            .await?;

        let mut stats = Vec::new();
        while let Some(row) = rows.next().await? {
            stats.push(Self::row_to_stats(&row)?);
        }

        Ok(stats)
    }

    fn row_to_tool_call(row: &turso::Row) -> Result<ToolCall> {
        let id = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0);

        let name = row
            .get_value(1)
            .ok()
            .and_then(|v| {
                if let Value::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let parameters = row.get_value(2).ok().and_then(|v| {
            if let Value::Text(s) = v {
                if !s.is_empty() {
                    serde_json::from_str(s.as_str()).ok()
                } else {
                    None
                }
            } else {
                None
            }
        });

        let result = row.get_value(3).ok().and_then(|v| {
            if let Value::Text(s) = v {
                if !s.is_empty() {
                    serde_json::from_str(s.as_str()).ok()
                } else {
                    None
                }
            } else {
                None
            }
        });

        let error = row.get_value(4).ok().and_then(|v| {
            if let Value::Text(s) = v {
                if !s.is_empty() {
                    Some(s.clone())
                } else {
                    None
                }
            } else {
                None
            }
        });

        let status = row
            .get_value(5)
            .ok()
            .and_then(|v| {
                if let Value::Text(s) = v {
                    Some(ToolCallStatus::from(s.as_str()))
                } else {
                    None
                }
            })
            .unwrap_or(ToolCallStatus::Pending);

        let started_at = row
            .get_value(6)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0);

        let completed_at = row.get_value(7).ok().and_then(|v| v.as_integer().copied());

        let duration_ms = row.get_value(8).ok().and_then(|v| v.as_integer().copied());

        Ok(ToolCall {
            id,
            name,
            parameters,
            result,
            error,
            status,
            started_at,
            completed_at,
            duration_ms,
        })
    }

    fn row_to_stats(row: &turso::Row) -> Result<ToolCallStats> {
        let name = row
            .get_value(0)
            .ok()
            .and_then(|v| {
                if let Value::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let total_calls = row
            .get_value(1)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0);

        let successful = row
            .get_value(2)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0);

        let failed = row
            .get_value(3)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0);

        let avg_duration_ms = row
            .get_value(4)
            .ok()
            .and_then(|v| match v {
                Value::Real(f) => Some(f),
                Value::Integer(i) => Some(i as f64),
                _ => None,
            })
            .unwrap_or(0.0);

        Ok(ToolCallStats {
            name,
            total_calls,
            successful,
            failed,
            avg_duration_ms,
        })
    }
}
