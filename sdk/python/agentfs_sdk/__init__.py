"""AgentFS Python SDK

A filesystem and key-value store for AI agents, powered by SQLite.
"""

from .agentfs import AgentFS, AgentFSOptions
from .errors import ErrnoException, FsErrorCode, FsSyscall
from .filesystem import S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, Filesystem, Stats
from .kvstore import KvStore
from .toolcalls import ToolCall, ToolCalls, ToolCallStats

__version__ = "0.4.0-pre.11"

__all__ = [
    "AgentFS",
    "AgentFSOptions",
    "KvStore",
    "Filesystem",
    "Stats",
    "S_IFDIR",
    "S_IFLNK",
    "S_IFMT",
    "S_IFREG",
    "ToolCalls",
    "ToolCall",
    "ToolCallStats",
    "ErrnoException",
    "FsErrorCode",
    "FsSyscall",
]
