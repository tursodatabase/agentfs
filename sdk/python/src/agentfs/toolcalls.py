"""Tool call tracking and statistics."""
import json
from dataclasses import dataclass
from typing import Any, Optional, List, Literal, TYPE_CHECKING

if TYPE_CHECKING:
    from .turso_async import AsyncTursoConnection


@dataclass
class ToolCall:
    """Recorded tool call."""
    id: int
    name: str
    parameters: Optional[Any]
    result: Optional[Any]
    error: Optional[str]
    status: Literal['pending', 'success', 'error']
    started_at: int
    completed_at: Optional[int]
    duration_ms: Optional[int]


@dataclass
class ToolCallStats:
    """Aggregated statistics for a tool."""
    name: str
    total_calls: int
    successful: int
    failed: int
    avg_duration_ms: float


class ToolCalls:
    """Async tool call tracking."""

    def __init__(self, db: "AsyncTursoConnection"):
        self._db = db
        self._ready = False

    async def ready(self) -> None:
        """Initialize database schema."""
        if self._ready:
            return

        await self._db.execute("""
            CREATE TABLE IF NOT EXISTS tool_calls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                parameters TEXT,
                result TEXT,
                error TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                duration_ms INTEGER
            )
        """)
        await self._db.execute("""
            CREATE INDEX IF NOT EXISTS idx_tool_calls_name
            ON tool_calls(name)
        """)
        await self._db.execute("""
            CREATE INDEX IF NOT EXISTS idx_tool_calls_started_at
            ON tool_calls(started_at)
        """)
        await self._db.commit()
        self._ready = True

    async def start(self, name: str, parameters: Optional[Any] = None) -> int:
        """Start tracking a tool call."""
        import time
        params_json = json.dumps(parameters) if parameters is not None else None
        cursor = await self._db.execute("""
            INSERT INTO tool_calls (name, parameters, started_at, status)
            VALUES (?, ?, ?, 'pending')
        """, (name, params_json, int(time.time())))
        await self._db.commit()
        return cursor.lastrowid

    async def success(self, call_id: int, result: Optional[Any] = None) -> None:
        """Mark tool call as successful."""
        import time
        result_json = json.dumps(result) if result is not None else None
        completed_at = int(time.time())

        # Get started_at to calculate duration
        cursor = await self._db.execute("""
            SELECT started_at FROM tool_calls WHERE id = ?
        """, (call_id,))
        row = await cursor.fetchone()
        if row:
            duration_ms = (completed_at - row[0]) * 1000
        else:
            duration_ms = None

        await self._db.execute("""
            UPDATE tool_calls
            SET status = 'success', result = ?, completed_at = ?, duration_ms = ?
            WHERE id = ?
        """, (result_json, completed_at, duration_ms, call_id))
        await self._db.commit()

    async def error(self, call_id: int, error: str) -> None:
        """Mark tool call as failed."""
        import time
        completed_at = int(time.time())

        # Get started_at to calculate duration
        cursor = await self._db.execute("""
            SELECT started_at FROM tool_calls WHERE id = ?
        """, (call_id,))
        row = await cursor.fetchone()
        if row:
            duration_ms = (completed_at - row[0]) * 1000
        else:
            duration_ms = None

        await self._db.execute("""
            UPDATE tool_calls
            SET status = 'error', error = ?, completed_at = ?, duration_ms = ?
            WHERE id = ?
        """, (error, completed_at, duration_ms, call_id))
        await self._db.commit()

    async def record(
        self,
        name: str,
        started_at: int,
        completed_at: int,
        parameters: Optional[Any] = None,
        result: Optional[Any] = None,
        error: Optional[str] = None
    ) -> int:
        """Record a completed tool call."""
        params_json = json.dumps(parameters) if parameters is not None else None
        result_json = json.dumps(result) if result is not None else None
        status = 'error' if error else 'success'
        duration_ms = (completed_at - started_at) * 1000

        cursor = await self._db.execute("""
            INSERT INTO tool_calls
            (name, parameters, result, error, status, started_at, completed_at, duration_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        """, (name, params_json, result_json, error, status, started_at, completed_at, duration_ms))
        await self._db.commit()
        return cursor.lastrowid

    async def get(self, call_id: int) -> Optional[ToolCall]:
        """Get a specific tool call."""
        cursor = await self._db.execute("""
            SELECT id, name, parameters, result, error, status,
                   started_at, completed_at, duration_ms
            FROM tool_calls WHERE id = ?
        """, (call_id,))
        row = await cursor.fetchone()
        if row is None:
            return None
        return self._row_to_toolcall(row)

    async def get_by_name(self, name: str, limit: int = 100) -> List[ToolCall]:
        """Get tool calls by name."""
        cursor = await self._db.execute("""
            SELECT id, name, parameters, result, error, status,
                   started_at, completed_at, duration_ms
            FROM tool_calls
            WHERE name = ?
            ORDER BY started_at DESC
            LIMIT ?
        """, (name, limit))
        rows = await cursor.fetchall()
        return [self._row_to_toolcall(row) for row in rows]

    async def get_recent(self, since: int, limit: int = 100) -> List[ToolCall]:
        """Get recent tool calls."""
        cursor = await self._db.execute("""
            SELECT id, name, parameters, result, error, status,
                   started_at, completed_at, duration_ms
            FROM tool_calls
            WHERE started_at >= ?
            ORDER BY started_at DESC
            LIMIT ?
        """, (since, limit))
        rows = await cursor.fetchall()
        return [self._row_to_toolcall(row) for row in rows]

    async def get_stats(self) -> List[ToolCallStats]:
        """Get aggregated statistics."""
        cursor = await self._db.execute("""
            SELECT
                name,
                COUNT(*) as total_calls,
                SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END) as successful,
                SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) as failed,
                AVG(CASE WHEN status != 'pending' THEN duration_ms ELSE NULL END) as avg_duration_ms
            FROM tool_calls
            GROUP BY name
        """)
        rows = await cursor.fetchall()
        return [
            ToolCallStats(
                name=row[0],
                total_calls=row[1],
                successful=row[2],
                failed=row[3],
                avg_duration_ms=row[4] or 0.0
            )
            for row in rows
        ]

    def _row_to_toolcall(self, row) -> ToolCall:
        """Convert database row to ToolCall."""
        return ToolCall(
            id=row[0],
            name=row[1],
            parameters=json.loads(row[2]) if row[2] else None,
            result=json.loads(row[3]) if row[3] else None,
            error=row[4],
            status=row[5],
            started_at=row[6],
            completed_at=row[7],
            duration_ms=row[8]
        )