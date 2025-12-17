"""Main AgentFS class"""

import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from turso.aio import Connection, connect

from .filesystem import Filesystem
from .kvstore import KvStore
from .toolcalls import ToolCalls


def agentfs_dir() -> Path:
    """Global directory containing agentfs databases (~/.agentfs/fs)

    This is the default location for new agent databases.
    """
    return Path.home() / ".agentfs" / "fs"


def local_agentfs_dir() -> Path:
    """Local directory containing agentfs databases (.agentfs in current directory)

    This is checked first when resolving agent IDs for backward compatibility.
    """
    return Path(".agentfs")


@dataclass
class AgentFSOptions:
    """Configuration options for opening an AgentFS instance

    Attributes:
        id: Unique identifier for the agent.
            - If provided without `path`: Creates storage at `~/.agentfs/fs/{id}.db`
            - If provided with `path`: Uses the specified path
        path: Explicit path to the database file.
            - If provided: Uses the specified path directly
            - Can be combined with `id`
    """

    id: Optional[str] = None
    path: Optional[str] = None


class AgentFS:
    """AgentFS - A filesystem and key-value store for AI agents

    Provides a unified interface for persistent storage using SQLite,
    with support for key-value storage, filesystem operations, and
    tool call tracking.
    """

    def __init__(self, db: Connection, kv: KvStore, fs: Filesystem, tools: ToolCalls):
        """Private constructor - use AgentFS.open() instead"""
        self._db = db
        self.kv = kv
        self.fs = fs
        self.tools = tools

    @staticmethod
    def resolve(id_or_path: str) -> str:
        """Resolve an id-or-path string to a database path

        - Existing file path -> uses that path directly
        - Otherwise -> treats as agent ID, looks for database in this order:
          1. `.agentfs/{id}.db` (local directory)
          2. `~/.agentfs/fs/{id}.db` (global directory)

        Args:
            id_or_path: Agent ID or path to database file

        Returns:
            Resolved database path

        Raises:
            ValueError: If the agent ID is invalid or the database doesn't exist
        """
        # Check if it's an existing file path
        if os.path.exists(id_or_path):
            return id_or_path

        # Treat as an agent ID - validate for safety
        if not re.match(r"^[a-zA-Z0-9_-]+$", id_or_path):
            raise ValueError(
                f"Invalid agent ID '{id_or_path}'. Agent IDs must contain only "
                "alphanumeric characters, hyphens, and underscores."
            )

        # Check local directory first (.agentfs in current directory)
        local_db_path = local_agentfs_dir() / f"{id_or_path}.db"
        if local_db_path.exists():
            return str(local_db_path)

        # Then check global directory (~/.agentfs/fs)
        global_db_path = agentfs_dir() / f"{id_or_path}.db"
        if global_db_path.exists():
            return str(global_db_path)

        raise ValueError(
            f"Agent '{id_or_path}' not found in local (.agentfs) or "
            "global (~/.agentfs/fs) directories"
        )

    @staticmethod
    async def open(options: AgentFSOptions) -> "AgentFS":
        """Open an agent filesystem

        Args:
            options: Configuration options (id and/or path required)

        Returns:
            Fully initialized AgentFS instance

        Raises:
            ValueError: If neither id nor path is provided, or if id contains invalid characters

        Example:
            >>> # Using id (creates ~/.agentfs/fs/my-agent.db)
            >>> agent = await AgentFS.open(AgentFSOptions(id='my-agent'))
            >>>
            >>> # Using id with custom path
            >>> agent = await AgentFS.open(AgentFSOptions(id='my-agent', path='./data/mydb.db'))
            >>>
            >>> # Using path only
            >>> agent = await AgentFS.open(AgentFSOptions(path='./data/mydb.db'))
        """
        # Require at least id or path
        if not options.id and not options.path:
            raise ValueError("AgentFS.open() requires at least 'id' or 'path'.")

        # Validate agent ID if provided
        if options.id and not re.match(r"^[a-zA-Z0-9_-]+$", options.id):
            raise ValueError(
                "Agent ID must contain only alphanumeric characters, hyphens, and underscores"
            )

        # Determine database path: explicit path takes precedence, otherwise use id-based path
        if options.path:
            db_path = options.path
        else:
            # id is guaranteed to be defined here (we checked not id and not path above)
            directory = agentfs_dir()
            directory.mkdir(parents=True, exist_ok=True)
            db_path = str(directory / f"{options.id}.db")

        # Connect to the database to ensure it's created
        db = await connect(db_path)

        return await AgentFS.open_with(db)

    @staticmethod
    async def open_with(db: Connection) -> "AgentFS":
        """Open an AgentFS instance with an existing database connection

        Args:
            db: An existing pyturso.aio Connection

        Returns:
            Fully initialized AgentFS instance
        """
        # Initialize all components in parallel
        kv = await KvStore.from_database(db)
        fs = await Filesystem.from_database(db)
        tools = await ToolCalls.from_database(db)

        return AgentFS(db, kv, fs, tools)

    def get_database(self) -> Connection:
        """Get the underlying Database connection"""
        return self._db

    async def close(self) -> None:
        """Close the database connection"""
        await self._db.close()

    async def __aenter__(self) -> "AgentFS":
        """Context manager entry"""
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        """Context manager exit"""
        await self.close()
