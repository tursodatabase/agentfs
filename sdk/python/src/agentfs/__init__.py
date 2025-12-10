"""AgentFS Python SDK - Key-value store, filesystem, and tool tracking."""
from typing import Optional
from .turso_async import AsyncTursoConnection, connect as turso_connect
from .kv import KvStore
from .filesystem import Filesystem
from .toolcalls import ToolCalls


class AgentFS:
    """Main AgentFS class with KV store, filesystem, and tool tracking."""

    def __init__(self, db_path: str = ':memory:'):
        """Initialize AgentFS with database path."""
        self._db_path = db_path
        self._db: Optional[AsyncTursoConnection] = None
        self._kv: Optional[KvStore] = None
        self._fs: Optional[Filesystem] = None
        self._tools: Optional[ToolCalls] = None
        self._ready = False

    async def ready(self) -> None:
        """Initialize database and all modules."""
        if self._ready:
            return

        self._db = await turso_connect(self._db_path)
        self._db.row_factory = None

        self._kv = KvStore(self._db)
        self._fs = Filesystem(self._db)
        self._tools = ToolCalls(self._db)

        await self._kv.ready()
        await self._fs.ready()
        await self._tools.ready()

        self._ready = True

    async def close(self) -> None:
        """Close database connection."""
        if self._db:
            await self._db.close()
            self._db = None
            self._ready = False

    @property
    def kv(self) -> KvStore:
        """Access key-value store."""
        if not self._kv:
            raise RuntimeError("AgentFS not initialized. Call await agentfs.ready() first.")
        return self._kv

    @property
    def fs(self) -> Filesystem:
        """Access filesystem."""
        if not self._fs:
            raise RuntimeError("AgentFS not initialized. Call await agentfs.ready() first.")
        return self._fs

    @property
    def tools(self) -> ToolCalls:
        """Access tool call tracking."""
        if not self._tools:
            raise RuntimeError("AgentFS not initialized. Call await agentfs.ready() first.")
        return self._tools

    def get_database(self) -> AsyncTursoConnection:
        """Get underlying database connection."""
        if not self._db:
            raise RuntimeError("AgentFS not initialized.")
        return self._db

    async def __aenter__(self):
        """Context manager entry."""
        await self.ready()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        """Context manager exit."""
        await self.close()


__all__ = ['AgentFS', 'KvStore', 'Filesystem', 'ToolCalls']