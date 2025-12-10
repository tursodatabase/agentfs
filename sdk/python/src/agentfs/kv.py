"""Key-value store with JSON serialization."""
import json
from typing import Any, Optional, TYPE_CHECKING

if TYPE_CHECKING:
    from .turso_async import AsyncTursoConnection


class KvStore:
    """Async key-value store backed by SQLite."""

    def __init__(self, db: "AsyncTursoConnection"):
        self._db = db
        self._ready = False

    async def ready(self) -> None:
        """Initialize database schema."""
        if self._ready:
            return

        await self._db.execute("""
            CREATE TABLE IF NOT EXISTS kv_store (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                created_at INTEGER DEFAULT (unixepoch()),
                updated_at INTEGER DEFAULT (unixepoch())
            )
        """)
        await self._db.execute("""
            CREATE INDEX IF NOT EXISTS idx_kv_store_created_at
            ON kv_store(created_at)
        """)
        await self._db.commit()
        self._ready = True

    async def set(self, key: str, value: Any) -> None:
        """Store a value with JSON serialization."""
        serialized = json.dumps(value)
        await self._db.execute("""
            INSERT INTO kv_store (key, value, updated_at)
            VALUES (?, ?, unixepoch())
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = unixepoch()
        """, (key, serialized))
        await self._db.commit()

    async def get(self, key: str) -> Optional[Any]:
        """Retrieve a value with JSON deserialization."""
        cursor = await self._db.execute(
            "SELECT value FROM kv_store WHERE key = ?",
            (key,)
        )
        row = await cursor.fetchone()
        if row is None:
            return None
        return json.loads(row[0])

    async def delete(self, key: str) -> None:
        """Delete a key."""
        await self._db.execute(
            "DELETE FROM kv_store WHERE key = ?",
            (key,)
        )
        await self._db.commit()