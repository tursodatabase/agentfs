---
allowed-tools: Bash(git add:*), Bash(git status:*), Bash(git commit:*), Bash(mkdir:*), Edit, Write
argument-hint: [ts-change-sha-commit]
description: Generate Python SDK for agentfs based on the Typescript SDK
---

## Context

- You must generate Python SDK with the API similar to the current Typescript SDK located at ../../sdk/typescript
- Last time, python sdk was updated based on the comment $1 (if value is "unspecified" then regenerate SDK from scratch)
- Use `pyturso.aio` python package which provide API similar to `aiosqlite`

```py
class Connection:
    def __init__(self, connector: Callable[[], BlockingConnection]) -> None:
    async def close(self) -> None:
    def __await__(self):
    async def __aenter__(self) -> "Connection":
    async def __aexit__(self, exc_type, exc, tb) -> None:
    def cursor(self, factory: Optional[Callable[[BlockingConnection], BlockingCursor]] = None) -> "Cursor":
    async def execute(self, sql: str, parameters: Sequence[Any] | Mapping[str, Any] = ()) -> "Cursor":
    async def executemany(self, sql: str, parameters: Iterable[Sequence[Any] | Mapping[str, Any]]) -> "Cursor":
    async def executescript(self, sql_script: str) -> "Cursor":
    async def commit(self) -> None:
    async def rollback(self) -> None:
class Cursor:
    async def close(self) -> None:
    # named parameters not supported at the moment
    async def execute(self, sql: str, parameters: Sequence[Any] | Mapping[str, Any] = ()) -> "Cursor":
    async def executemany(self, sql: str, parameters: Iterable[Sequence[Any] | Mapping[str, Any]]) -> "Cursor":
    async def executescript(self, sql_script: str) -> "Cursor":
    async def fetchone(self) -> Any:
    async def fetchmany(self, size: Optional[int] = None) -> list[Any]:
    async def fetchall(self) -> list[Any]:
    async def __aenter__(self) -> "Cursor":
    async def __aexit__(self, exc_type, exc, tb) -> None:

def connect(database: str) -> Connection:
```
