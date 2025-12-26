---
allowed-tools: Bash(git commit:*), Bash(git add:*), Bash(git status:*), Bash(mkdir:*), Bash(uv:*), Read, Edit(sdk/python/**), Write(sdk/python/**), Edit(.github/workflows/python.yml), Write(.github/workflows/python.yml), Write(.github/workflows/release.yml), Edit(.github/workflows/release.yml)
argument-hint: [ts-change-sha-commit]
description: Generate Python SDK for agentfs based on the Typescript SDK
---

## Dev rules

- FRESH RULES from this file have higher priority than any other rules if they conflict
- YOU MUST COMMIT your changes FREQUENTLY DURING the process with compact but informative message with the motivation for the change and its high level description
  - Don't hesitate to commit partial progress
- USE `uv` with `--directory sdk/python` command in order to avoid `cd` to the subdirectory
- ALWAYS USE pathes relative to the project root
- DO NOT EVER `cd` into the directories - tool permissions will not be validated properly
- USE ONLY SIMPLE "ls", "grep", "find", "cat" Bash commands and native Claude Code tools - otherwise permission will be blocked
- DOCUMENT tricky and hacky moments in the code

## Context

- Last time, python sdk was updated based on the comment $1

  - If value is "unspecified" then regenerate SDK from scratch
  - If value is set - FOCUS on the diff between the current state and specified commit hash
    - The primary changes are in the Typescript SDK but changes outside of it also can contribute to the process
    - For example, command prompt in .claude directory influence process heavily

- You must generate Python SDK with the API similar to the current Typescript SDK located at ../../sdk/typescript
- The package name is `agentfs-sdk` and import path must be `agentfs_sdk`
- You must transfer all tests from Typescript SDK to the Python
- Use `turso.aio` python package which provide API similar to `aiosqlite`
- Use simple setup with builtin uv ruff formatter
- Use pytest for testing
- Use ty for type checking
- Maintain CI for linting and checking at .github/workflows/python.yml similar to the TS workflow at .github/workflows/typescript.yml
- Maintain CI for publishing the Python package to the PyPI in the .github/workflows/release.yml
  - Use `PYPI_API_TOKEN` secret
- In the agetnfs-sdk implementation always explicitly close cursor or use it as context manager
- The SDK must work properly when CDC is enabled for tursodb: cover this scenario with additional test suite
  - Execute `PRAGMA unstable_capture_data_changes_conn('full')` pragma to enable CDC for connection

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

# as Connection is awaitable - caller can use await connect(...)
def connect(database: str) -> Connection:
```
