"""Async wrapper for synchronous libsql library.

Uses a dedicated thread for all database operations to avoid
thread-safety issues with Rust bindings.
"""
import asyncio
import queue
import threading
from typing import Any, Optional, Tuple, List
from dataclasses import dataclass

import libsql


@dataclass
class _Request:
    """Request to execute on the DB thread."""
    method: str
    args: tuple
    result_queue: queue.Queue


class _DBThread(threading.Thread):
    """Dedicated thread for all database operations."""

    def __init__(self, db_path: str):
        super().__init__(daemon=True)
        self._db_path = db_path
        self._request_queue: queue.Queue = queue.Queue()
        self._conn = None
        self._running = True

    def run(self):
        """Main thread loop - process requests sequentially."""
        self._conn = libsql.connect(self._db_path)

        while self._running:
            try:
                request = self._request_queue.get(timeout=0.1)
            except queue.Empty:
                continue

            if request.method == "_shutdown":
                break

            try:
                result = self._handle_request(request)
                request.result_queue.put(("ok", result))
            except Exception as e:
                request.result_queue.put(("error", e))

        # Cleanup on same thread
        if self._conn:
            self._conn.close()

    def _handle_request(self, request: _Request):
        """Handle a single request."""
        method = request.method
        args = request.args

        if method == "execute":
            sql, params = args
            cur = self._conn.execute(sql, params)
            # Get lastrowid from cursor
            lastrowid = cur.lastrowid
            return ("cursor", cur, lastrowid)

        elif method == "fetchone":
            cursor = args[0]
            return cursor.fetchone()

        elif method == "fetchall":
            cursor = args[0]
            return cursor.fetchall()

        elif method == "commit":
            self._conn.commit()
            return None

        elif method == "close":
            self._running = False
            return None

        else:
            raise ValueError(f"Unknown method: {method}")

    def submit(self, method: str, *args) -> Any:
        """Submit request and wait for result."""
        result_queue: queue.Queue = queue.Queue()
        request = _Request(method=method, args=args, result_queue=result_queue)
        self._request_queue.put(request)
        status, result = result_queue.get()
        if status == "error":
            raise result
        return result


class AsyncTursoCursor:
    """Async wrapper for libsql cursor."""

    def __init__(self, db_thread: _DBThread, cursor, lastrowid: Optional[int] = None):
        self._db_thread = db_thread
        self._cursor = cursor
        self.lastrowid: Optional[int] = lastrowid

    async def fetchone(self) -> Optional[Tuple]:
        """Fetch one row asynchronously."""
        loop = asyncio.get_event_loop()
        return await loop.run_in_executor(
            None, self._db_thread.submit, "fetchone", self._cursor
        )

    async def fetchall(self) -> List[Tuple]:
        """Fetch all rows asynchronously."""
        loop = asyncio.get_event_loop()
        return await loop.run_in_executor(
            None, self._db_thread.submit, "fetchall", self._cursor
        )


class AsyncTursoConnection:
    """Async wrapper for libsql connection.

    All database operations run on a dedicated thread to avoid
    thread-safety issues with Rust bindings.
    """

    def __init__(self, db_thread: _DBThread):
        self._db_thread = db_thread
        self.row_factory = None

    @classmethod
    async def connect(cls, db_path: str) -> "AsyncTursoConnection":
        """Create async connection to database."""
        db_thread = _DBThread(db_path)
        db_thread.start()
        return cls(db_thread)

    async def execute(self, sql: str, params: Tuple = ()) -> AsyncTursoCursor:
        """Execute SQL statement asynchronously."""
        loop = asyncio.get_event_loop()
        result = await loop.run_in_executor(
            None, self._db_thread.submit, "execute", sql, params
        )
        _, cursor, lastrowid = result
        return AsyncTursoCursor(self._db_thread, cursor, lastrowid)

    async def commit(self) -> None:
        """Commit transaction asynchronously."""
        loop = asyncio.get_event_loop()
        await loop.run_in_executor(None, self._db_thread.submit, "commit")

    async def close(self) -> None:
        """Close connection asynchronously."""
        loop = asyncio.get_event_loop()
        await loop.run_in_executor(None, self._db_thread.submit, "close")
        self._db_thread.join(timeout=1.0)


async def connect(db_path: str) -> AsyncTursoConnection:
    """Connect to database asynchronously."""
    return await AsyncTursoConnection.connect(db_path)
