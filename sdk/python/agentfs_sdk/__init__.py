"""AgentFS Python SDK

A filesystem and key-value store for AI agents, powered by SQLite.
"""

from .agentfs import AgentFS, AgentFSOptions, agentfs_dir, local_agentfs_dir
from .filesystem import Filesystem, Stats
from .kvstore import KvStore
from .toolcalls import ToolCall, ToolCalls, ToolCallStats

__version__ = "0.3.0-pre.7"

__all__ = [
    "AgentFS",
    "AgentFSOptions",
    "agentfs_dir",
    "local_agentfs_dir",
    "KvStore",
    "Filesystem",
    "Stats",
    "ToolCalls",
    "ToolCall",
    "ToolCallStats",
]
