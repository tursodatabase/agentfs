# AgentFS Python SDK

A Python SDK for AgentFS - providing a key-value store, Unix-like filesystem, and tool call tracking backed by SQLite.

## Features

- **Async API**: Built with asyncio for high-performance concurrent operations
- **Key-Value Store**: JSON-serialized key-value pairs with automatic timestamps
- **Unix-like Filesystem**: Complete filesystem with inodes, directories, and file operations
- **Tool Call Tracking**: Track and analyze tool usage with timing and statistics
- **SQLite Backend**: Persistent, ACID-compliant storage with a single database file
- **Context Manager**: Easy resource management with `async with` support
- **Type Hints**: Full type annotation support for better IDE integration

## Installation

```bash
# From source
git clone <repository-url>
cd agentfs/sdk/python
uv pip install -e ".[dev]"

# Or install from pip (when published)
pip install agentfs
```

## Quick Start

```python
import asyncio
from agentfs import AgentFS

async def main():
    async with AgentFS() as agent:
        # Key-value store operations
        await agent.kv.set('user:1', {'name': 'Alice', 'age': 30})
        user = await agent.kv.get('user:1')
        print(f"User: {user}")

        # Filesystem operations
        await agent.fs.write_file('/data/config.json', '{"debug": true}')
        config = await agent.fs.read_file('/data/config.json')
        print(f"Config: {config}")

        # Tool call tracking
        call_id = await agent.tools.start('data_processing')
        # ... do work ...
        await agent.tools.success(call_id, {'processed': 100})

        stats = await agent.tools.get_stats()
        print(f"Tool stats: {stats}")

asyncio.run(main())
```

## Modules

### KvStore - Key-Value Storage

```python
async with AgentFS() as agent:
    # Store different data types
    await agent.kv.set('username', 'alice')
    await agent.kv.set('count', 42)
    await agent.kv.set('user:1', {'id': 1, 'name': 'Alice'})

    # Retrieve values
    username = await agent.kv.get('username')  # 'alice'
    user = await agent.kv.get('user:1')       # {'id': 1, 'name': 'Alice'}

    # Delete keys
    await agent.kv.delete('username')
```

### Filesystem - Unix-like File Operations

```python
async with AgentFS() as agent:
    # Write files (creates parent directories automatically)
    await agent.fs.write_file('/logs/app.log', 'Application started')
    await agent.fs.write_file('/config/settings.json', '{"debug": true}')

    # Read files
    content = await agent.fs.read_file('/logs/app.log')

    # List directory contents
    files = await agent.fs.readdir('/config')

    # Get file statistics
    stats = await agent.fs.stat('/logs/app.log')
    print(f"Size: {stats.size} bytes")
    print(f"Is file: {stats.is_file()}")
    print(f"Modified: {stats.mtime}")

    # Delete files
    await agent.fs.delete_file('/logs/app.log')
```

### ToolCalls - Tool Usage Tracking

```python
async with AgentFS() as agent:
    # Start tracking a tool call
    call_id = await agent.tools.start('web_search', {'query': 'python async'})

    # Mark as successful
    await agent.tools.success(call_id, {'results': 10})

    # Or mark as failed
    await agent.tools.error(call_id, 'Network timeout')

    # Get specific call details
    call = await agent.tools.get(call_id)

    # Get calls by tool name
    search_calls = await agent.tools.get_by_name('web_search')

    # Get aggregated statistics
    stats = await agent.tools.get_stats()
    for stat in stats:
        print(f"{stat.name}: {stat.total_calls} calls, "
              f"{stat.successful} success, {stat.failed} failed, "
              f"{stat.avg_duration_ms:.0f}ms avg")
```

## Database Persistence

```python
# In-memory database (default)
async with AgentFS() as agent:
    # Data is lost when the session ends
    await agent.kv.set('temp', 'value')

# File-based database
async with AgentFS('/path/to/database.db') as agent:
    # Data persists across sessions
    await agent.kv.set('persistent', 'value')

# Manual initialization
agent = AgentFS('/path/to/database.db')
await agent.ready()
# ... use agent ...
await agent.close()
```

## Examples

The `examples/` directory contains detailed usage examples:

- `filesystem_example.py` - File operations and directory management
- `kvstore_example.py` - Key-value store CRUD operations
- `toolcalls_example.py` - Tool call tracking and statistics

Run examples:
```bash
uv run python examples/filesystem_example.py
uv run python examples/kvstore_example.py
uv run python examples/toolcalls_example.py
```

## Development

```bash
# Install development dependencies
uv pip install -e ".[dev]"

# Run tests
uv run pytest -v

# Run tests with coverage
uv run pytest --cov=src/agentfs --cov-report=term --cov-report=html -v

# Run specific test module
uv run pytest tests/test_kv.py -v
```

## Database Schema

AgentFS uses a single SQLite database with four main tables:

- `kv_store` - Key-value pairs with JSON serialization
- `fs_inode` - Filesystem inodes (files and directories)
- `fs_dentry` - Directory entries linking names to inodes
- `fs_data` - File data storage
- `tool_calls` - Tool call tracking with timing and results

## API Reference

### AgentFS

Main class providing access to all modules.

```python
class AgentFS:
    def __init__(self, db_path: str = ':memory:'):
        """Initialize with database path."""

    async def ready(self) -> None:
        """Initialize database and modules."""

    async def close(self) -> None:
        """Close database connection."""

    @property
    def kv(self) -> KvStore:
        """Access key-value store."""

    @property
    def fs(self) -> Filesystem:
        """Access filesystem."""

    @property
    def tools(self) -> ToolCalls:
        """Access tool call tracking."""
```

### KvStore

Async key-value store with JSON serialization.

- `async def set(self, key: str, value: Any) -> None`
- `async def get(self, key: str) -> Optional[Any]`
- `async def delete(self, key: str) -> None`

### Filesystem

Unix-like filesystem with inode-based structure.

- `async def write_file(self, path: str, content: str | bytes) -> None`
- `async def read_file(self, path: str) -> str`
- `async def readdir(self, path: str) -> List[str]`
- `async def stat(self, path: str) -> Stats`
- `async def delete_file(self, path: str) -> None`

### ToolCalls

Tool call tracking and statistics.

- `async def start(self, name: str, parameters: Optional[Any] = None) -> int`
- `async def success(self, call_id: int, result: Optional[Any] = None) -> None`
- `async def error(self, call_id: int, error: str) -> None`
- `async def get(self, call_id: int) -> Optional[ToolCall]`
- `async def get_by_name(self, name: str, limit: int = 100) -> List[ToolCall]`
- `async def get_stats(self) -> List[ToolCallStats]`

## License

[Add your license information here]

## Contributing

[Add contribution guidelines here]