import pytest
from agentfs.turso_async import connect
from agentfs.filesystem import Filesystem, Stats


@pytest.fixture
async def filesystem():
    """Create temporary filesystem for testing."""
    db = await connect(':memory:')
    fs = Filesystem(db)
    await fs.ready()
    yield fs
    await db.close()


@pytest.mark.asyncio
async def test_write_and_read_file(filesystem):
    await filesystem.write_file('/test.txt', 'Hello, World!')
    content = await filesystem.read_file('/test.txt')
    assert content == 'Hello, World!'


@pytest.mark.asyncio
async def test_write_nested_file_creates_parents(filesystem):
    await filesystem.write_file('/dir1/dir2/file.txt', 'content')
    content = await filesystem.read_file('/dir1/dir2/file.txt')
    assert content == 'content'


@pytest.mark.asyncio
async def test_read_nonexistent_raises_error(filesystem):
    with pytest.raises(FileNotFoundError):
        await filesystem.read_file('/nonexistent.txt')


@pytest.mark.asyncio
async def test_readdir_lists_files(filesystem):
    await filesystem.write_file('/dir/file1.txt', 'a')
    await filesystem.write_file('/dir/file2.txt', 'b')
    files = await filesystem.readdir('/dir')
    assert set(files) == {'file1.txt', 'file2.txt'}


@pytest.mark.asyncio
async def test_stat_file(filesystem):
    await filesystem.write_file('/test.txt', 'content')
    stats = await filesystem.stat('/test.txt')
    assert stats.is_file()
    assert not stats.is_directory()
    assert stats.size == 7  # len('content')


@pytest.mark.asyncio
async def test_stat_directory(filesystem):
    await filesystem.write_file('/dir/file.txt', 'x')
    stats = await filesystem.stat('/dir')
    assert stats.is_directory()
    assert not stats.is_file()


@pytest.mark.asyncio
async def test_delete_file(filesystem):
    await filesystem.write_file('/temp.txt', 'data')
    await filesystem.delete_file('/temp.txt')
    with pytest.raises(FileNotFoundError):
        await filesystem.read_file('/temp.txt')