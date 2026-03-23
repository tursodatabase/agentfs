"""Tests for filesystem search extensions: grep, find, wc"""

import os
import tempfile

import pytest
from turso.aio import connect

from agentfs_sdk import Filesystem, find, grep, wc


async def make_fs():
    tmpdir = tempfile.mkdtemp()
    db_path = os.path.join(tmpdir, "test.db")
    db = await connect(db_path)
    await db.execute("PRAGMA unstable_capture_data_changes_conn('full')")
    fs = await Filesystem.from_database(db)
    return fs, db


# ── grep ──────────────────────────────────────────────────────────────────────


class TestGrep:
    async def test_basic_match(self):
        fs, db = await make_fs()
        await fs.write_file("/file.txt", "hello world\nfoo bar\nhello again")
        result = await grep(fs, "/file.txt", "hello")
        assert result.count == 2
        assert result.matches[0].line == "hello world"
        assert result.matches[1].line == "hello again"
        await db.close()

    async def test_no_match(self):
        fs, db = await make_fs()
        await fs.write_file("/file.txt", "hello world")
        result = await grep(fs, "/file.txt", "missing")
        assert result.count == 0
        assert result.matches == []
        await db.close()

    async def test_ignore_case(self):
        fs, db = await make_fs()
        await fs.write_file("/file.txt", "Hello World\nhello world")
        result = await grep(fs, "/file.txt", "hello", ignore_case=True)
        assert result.count == 2
        await db.close()

    async def test_line_number(self):
        fs, db = await make_fs()
        await fs.write_file("/file.txt", "line one\nline two\nline three")
        result = await grep(fs, "/file.txt", "two", line_number=True)
        assert result.count == 1
        assert result.matches[0].line_number == 2
        await db.close()

    async def test_count_only(self):
        fs, db = await make_fs()
        await fs.write_file("/file.txt", "foo\nfoo\nbar")
        result = await grep(fs, "/file.txt", "foo", count=True)
        assert result.count == 2
        assert result.matches == []
        await db.close()

    async def test_fixed_string(self):
        fs, db = await make_fs()
        await fs.write_file("/file.txt", "price: $10.00\nprice: $20.00")
        # Without fixed_string, "." is a regex wildcard
        result = await grep(fs, "/file.txt", "$10.00", fixed_string=True)
        assert result.count == 1
        await db.close()

    async def test_binary_file(self):
        fs, db = await make_fs()
        await fs.write_file("/file.bin", b"binary\x00data", encoding=None)
        result = await grep(fs, "/file.bin", "binary")
        assert result.message is not None
        assert result.matches == []
        await db.close()

    async def test_file_not_found(self):
        fs, db = await make_fs()
        with pytest.raises(Exception):
            await grep(fs, "/nonexistent.txt", "pattern")
        await db.close()


# ── find ──────────────────────────────────────────────────────────────────────


class TestFind:
    async def test_find_all(self):
        fs, db = await make_fs()
        await fs.write_file("/a.txt", "a")
        await fs.write_file("/b.py", "b")
        result = await find(fs, "/")
        assert "/a.txt" in result.paths
        assert "/b.py" in result.paths
        await db.close()

    async def test_find_by_extension_full_path(self):
        fs, db = await make_fs()
        await fs.write_file("/a.txt", "a")
        await fs.write_file("/b.py", "b")
        result = await find(fs, "/", "*.py")
        assert "/b.py" in result.paths
        assert "/a.txt" not in result.paths
        await db.close()

    async def test_find_name_only(self):
        fs, db = await make_fs()
        await fs.write_file("/src/main.py", "main")
        await fs.write_file("/src/util.py", "util")
        await fs.write_file("/src/readme.txt", "readme")
        result = await find(fs, "/", "*.py", name_only=True)
        assert "/src/main.py" in result.paths
        assert "/src/util.py" in result.paths
        assert "/src/readme.txt" not in result.paths
        await db.close()

    async def test_find_type_files_only(self):
        fs, db = await make_fs()
        await fs.write_file("/dir/file.txt", "content")
        result = await find(fs, "/", entry_type="f")
        assert "/dir/file.txt" in result.paths
        assert "/dir" not in result.paths
        await db.close()

    async def test_find_type_dirs_only(self):
        fs, db = await make_fs()
        await fs.write_file("/dir/file.txt", "content")
        result = await find(fs, "/", entry_type="d")
        assert "/dir" in result.paths
        assert "/dir/file.txt" not in result.paths
        await db.close()

    async def test_find_recursive(self):
        fs, db = await make_fs()
        await fs.write_file("/a/b/c/deep.txt", "deep")
        result = await find(fs, "/", "*.txt")
        assert "/a/b/c/deep.txt" in result.paths
        await db.close()

    async def test_find_includes_root(self):
        fs, db = await make_fs()
        result = await find(fs, "/", name_only=True)
        assert "/" in result.paths
        await db.close()


# ── wc ────────────────────────────────────────────────────────────────────────


class TestWc:
    async def test_basic_counts(self):
        fs, db = await make_fs()
        await fs.write_file("/file.txt", "hello world\nfoo bar\n")
        result = await wc(fs, "/file.txt")
        assert result.lines == 2
        assert result.words == 4
        assert result.bytes == 20
        await db.close()

    async def test_empty_file(self):
        fs, db = await make_fs()
        await fs.write_file("/empty.txt", "")
        result = await wc(fs, "/empty.txt")
        assert result.lines == 0
        assert result.words == 0
        assert result.bytes == 0
        await db.close()

    async def test_binary_file(self):
        fs, db = await make_fs()
        await fs.write_file("/file.bin", b"binary\x00data", encoding=None)
        result = await wc(fs, "/file.bin")
        assert result.bytes == 11
        assert result.message is not None
        await db.close()

    async def test_file_not_found(self):
        fs, db = await make_fs()
        with pytest.raises(Exception):
            await wc(fs, "/nonexistent.txt")
        await db.close()
