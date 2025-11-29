import pytest
from agentfs.turso_async import connect
from agentfs.kv import KvStore


@pytest.fixture
async def kv_store():
    """Create temporary KvStore for testing."""
    db = await connect(':memory:')
    store = KvStore(db)
    await store.ready()
    yield store
    await db.close()


@pytest.mark.asyncio
async def test_set_and_get_string(kv_store):
    await kv_store.set('key1', 'value1')
    result = await kv_store.get('key1')
    assert result == 'value1'


@pytest.mark.asyncio
async def test_get_nonexistent_returns_none(kv_store):
    result = await kv_store.get('nonexistent')
    assert result is None


@pytest.mark.asyncio
async def test_set_and_get_object(kv_store):
    data = {'name': 'Alice', 'age': 30}
    await kv_store.set('user:1', data)
    result = await kv_store.get('user:1')
    assert result == data


@pytest.mark.asyncio
async def test_delete_key(kv_store):
    await kv_store.set('temp', 'value')
    await kv_store.delete('temp')
    result = await kv_store.get('temp')
    assert result is None


@pytest.mark.asyncio
async def test_update_existing_key(kv_store):
    await kv_store.set('key', 'old')
    await kv_store.set('key', 'new')
    result = await kv_store.get('key')
    assert result == 'new'