import pytest
import tempfile
from pathlib import Path
from agentfs import AgentFS


@pytest.mark.asyncio
async def test_agentfs_memory_initialization():
    """Test in-memory database initialization."""
    async with AgentFS() as agent:
        assert agent.kv is not None
        assert agent.fs is not None
        assert agent.tools is not None


@pytest.mark.asyncio
async def test_agentfs_file_initialization():
    """Test file-based database initialization."""
    with tempfile.NamedTemporaryFile(suffix='.db', delete=False) as f:
        db_path = f.name

    try:
        async with AgentFS(db_path) as agent:
            await agent.kv.set('test', 'value')

        # Verify persistence
        async with AgentFS(db_path) as agent:
            value = await agent.kv.get('test')
            assert value == 'value'
    finally:
        Path(db_path).unlink(missing_ok=True)


@pytest.mark.asyncio
async def test_agentfs_all_modules_work():
    """Test that all modules work together."""
    async with AgentFS() as agent:
        # KvStore
        await agent.kv.set('key', 'value')
        assert await agent.kv.get('key') == 'value'

        # Filesystem
        await agent.fs.write_file('/test.txt', 'content')
        assert await agent.fs.read_file('/test.txt') == 'content'

        # ToolCalls
        import time
        call_id = await agent.tools.start('test_tool')
        await agent.tools.success(call_id, {'result': 'ok'})
        call = await agent.tools.get(call_id)
        assert call.status == 'success'


@pytest.mark.asyncio
async def test_agentfs_without_context_manager():
    """Test manual initialization and cleanup."""
    agent = AgentFS()
    await agent.ready()
    await agent.kv.set('test', 123)
    assert await agent.kv.get('test') == 123
    await agent.close()