import pytest
import time
from agentfs.turso_async import connect
from agentfs.toolcalls import ToolCalls, ToolCall, ToolCallStats


@pytest.fixture
async def toolcalls():
    """Create temporary ToolCalls for testing."""
    db = await connect(':memory:')
    tc = ToolCalls(db)
    await tc.ready()
    yield tc
    await db.close()


@pytest.mark.asyncio
async def test_start_and_success(toolcalls):
    call_id = await toolcalls.start('test_tool', {'arg': 'value'})
    assert call_id > 0
    await toolcalls.success(call_id, {'result': 'ok'})
    call = await toolcalls.get(call_id)
    assert call.status == 'success'
    assert call.result == {'result': 'ok'}


@pytest.mark.asyncio
async def test_start_and_error(toolcalls):
    call_id = await toolcalls.start('failing_tool')
    await toolcalls.error(call_id, 'Something went wrong')
    call = await toolcalls.get(call_id)
    assert call.status == 'error'
    assert call.error == 'Something went wrong'


@pytest.mark.asyncio
async def test_record_complete_call(toolcalls):
    start_time = int(time.time())
    end_time = start_time + 2
    call_id = await toolcalls.record(
        'complete_tool',
        start_time,
        end_time,
        parameters={'x': 1},
        result={'y': 2}
    )
    call = await toolcalls.get(call_id)
    assert call.status == 'success'
    assert call.duration_ms == 2000


@pytest.mark.asyncio
async def test_get_by_name(toolcalls):
    await toolcalls.record('tool_a', int(time.time()), int(time.time()))
    await toolcalls.record('tool_b', int(time.time()), int(time.time()))
    await toolcalls.record('tool_a', int(time.time()), int(time.time()))

    calls = await toolcalls.get_by_name('tool_a')
    assert len(calls) == 2
    assert all(c.name == 'tool_a' for c in calls)


@pytest.mark.asyncio
async def test_get_stats(toolcalls):
    start = int(time.time())
    await toolcalls.record('tool1', start, start + 1, result={'ok': True})
    await toolcalls.record('tool1', start, start + 3, result={'ok': True})
    await toolcalls.record('tool1', start, start + 2, error='failed')

    stats = await toolcalls.get_stats()
    tool1_stats = next(s for s in stats if s.name == 'tool1')
    assert tool1_stats.total_calls == 3
    assert tool1_stats.successful == 2
    assert tool1_stats.failed == 1
    assert tool1_stats.avg_duration_ms == 2000  # (1000 + 3000) / 2