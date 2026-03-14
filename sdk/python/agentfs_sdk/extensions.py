"""Filesystem search extensions for AgentFS

Provides grep, find, and wc operations on top of the core AgentFS filesystem.

Optional dependency: google-re2 (install with `pip install agentfs-sdk[re2]`).
If not installed, grep falls back to stdlib re, which is vulnerable to ReDoS
on untrusted patterns.
"""

import fnmatch
import re
import warnings
from dataclasses import dataclass, field
from typing import List, Literal, Optional

from .filesystem import Filesystem

try:
    import re2  # type: ignore[import-not-found]

    _RE2_AVAILABLE = True
except ImportError:
    _RE2_AVAILABLE = False


# ── grep ──────────────────────────────────────────────────────────────────────


@dataclass
class GrepMatch:
    """A single grep match

    Attributes:
        line: The full line containing the match
        line_number: Line number (1-indexed), populated when line_number=True
    """

    line: str
    line_number: Optional[int] = None


@dataclass
class GrepResult:
    """Result of a grep operation

    Attributes:
        matches: List of matching lines
        count: Number of matches
        message: Informational message (e.g., binary file warning)
    """

    matches: List[GrepMatch] = field(default_factory=list)
    count: int = 0
    message: Optional[str] = None


def _is_binary(data: bytes) -> bool:
    return b"\x00" in data


async def grep(
    fs: Filesystem,
    path: str,
    pattern: str,
    ignore_case: bool = False,
    line_number: bool = False,
    count: bool = False,
    fixed_string: bool = False,
) -> GrepResult:
    """Search file contents for lines matching a pattern

    Behaves like the standard Unix grep command. Searches a single file for
    lines matching a pattern. Uses the RE2 engine when available (linear time,
    no backtracking), falling back to stdlib re with a warning otherwise.

    Args:
        fs: AgentFS Filesystem instance
        path: Path to the file to search
        pattern: Pattern to match. Treated as a regular expression by default.
        ignore_case: Case-insensitive matching (like grep -i)
        line_number: Include line numbers in results (like grep -n)
        count: Only return the match count, not the matches (like grep -c)
        fixed_string: Treat pattern as a literal string, not a regex (like grep -F)

    Returns:
        GrepResult with matches, count, and optional message.
        If the file is binary, returns GrepResult with a message and no matches.

    Raises:
        ErrnoException: ENOENT if file does not exist
        ErrnoException: EISDIR if path is a directory
        re2.error: If pattern is an invalid regular expression

    Example:
        >>> result = await grep(fs, '/data/app.py', 'import')
        >>> for match in result.matches:
        ...     print(match.line)
    """
    raw = await fs.read_file(path, encoding=None)
    assert isinstance(raw, bytes)

    if _is_binary(raw):
        return GrepResult(message=f"Binary file {path}, cannot search")

    content = raw.decode("utf-8")

    if not _RE2_AVAILABLE:
        warnings.warn(
            "google-re2 is not installed. Falling back to stdlib re, which is vulnerable "
            "to ReDoS on untrusted patterns. Install google-re2 for safe regex matching.",
            RuntimeWarning,
            stacklevel=2,
        )

    safe_pattern = re.escape(pattern) if fixed_string else pattern

    if _RE2_AVAILABLE:
        opts = re2.Options()  # type: ignore[possibly-undefined]
        opts.case_sensitive = not ignore_case
        compiled = re2.compile(safe_pattern, opts)  # type: ignore[possibly-undefined]
    else:
        # TODO: add timeout protection for re fallback — compile and search can both
        # hang on pathological patterns from untrusted input (ReDoS). signal.SIGALRM
        # or threading with a timeout would be appropriate here.
        flags = re.IGNORECASE if ignore_case else 0
        compiled = re.compile(safe_pattern, flags)

    match_count = 0
    matches = []
    for i, line in enumerate(content.splitlines(), 1):
        if compiled.search(line):
            match_count += 1
            if not count:
                ln = i if line_number else None
                matches.append(GrepMatch(line=line, line_number=ln))

    if count:
        return GrepResult(count=match_count)

    return GrepResult(matches=matches, count=match_count)


# ── find ──────────────────────────────────────────────────────────────────────


@dataclass
class FindResult:
    """Result of a find operation

    Attributes:
        paths: List of matching paths
        count: Number of matches
    """

    paths: List[str] = field(default_factory=list)
    count: int = 0


async def find(
    fs: Filesystem,
    path: str,
    pattern: str = "*",
    entry_type: Optional[Literal["f", "d"]] = None,
    name_only: bool = False,
) -> FindResult:
    """Search for files and directories by pattern

    Recursively walks the directory tree from path. By default matches the
    pattern against the full path (like find -path). Use name_only=True to
    match against the filename only (like find -name).

    The root path itself is included as a candidate, matching real find behavior.

    Args:
        fs: AgentFS Filesystem instance
        path: Root directory to search from
        pattern: Pattern to match. Matched against full path by default, or
                 against the entry name alone when name_only=True.
                 Supports glob syntax: *, ?, [abc].
        entry_type: Filter by entry type — "f" for files only, "d" for directories only.
                    If None, returns both.
        name_only: When True, match pattern against the entry name only (like find -name).
                   When False (default), match against the full path (like find -path).

    Returns:
        FindResult with matching paths and count.

    Raises:
        ErrnoException: ENOENT if path does not exist
        ErrnoException: ENOTDIR if path is not a directory

    Example:
        >>> # match full path — finds anything ending in .py anywhere
        >>> result = await find(fs, '/', '*.py')

        >>> # match name only — equivalent to find / -name "*.py"
        >>> result = await find(fs, '/', '*.py', name_only=True)
    """
    matches: List[str] = []

    def _matches(entry_path: str, name: str) -> bool:
        subject = name if name_only else entry_path
        return fnmatch.fnmatch(subject, pattern)

    def _include(entry_path: str, stats) -> bool:
        if entry_type is None:
            return True
        if entry_type == "f":
            return stats.is_file()
        if entry_type == "d":
            return stats.is_directory()
        return False

    async def _walk(current: str) -> None:
        entries = await fs.readdir(current)
        for name in entries:
            entry_path = f"{current.rstrip('/')}/{name}"
            stats = await fs.stat(entry_path)

            if _matches(entry_path, name) and _include(entry_path, stats):
                matches.append(entry_path)

            if stats.is_directory():
                await _walk(entry_path)

    # include the root itself as a candidate, matching real find behavior
    root_stats = await fs.stat(path)
    root_name = path.rstrip("/").rsplit("/", 1)[-1] or "/"
    if _matches(path, root_name) and _include(path, root_stats):
        matches.append(path)

    await _walk(path)
    return FindResult(paths=matches, count=len(matches))


# ── wc ────────────────────────────────────────────────────────────────────────


@dataclass
class WcResult:
    """Result of a wc operation

    Attributes:
        lines: Number of newline characters (like wc -l)
        words: Number of whitespace-delimited words (like wc -w)
        bytes: File size in bytes (like wc -c)
        message: Informational message (e.g., binary file warning)
    """

    lines: int = 0
    words: int = 0
    bytes: int = 0
    message: Optional[str] = None


async def wc(fs: Filesystem, path: str) -> WcResult:
    """Count lines, words, and bytes in a file

    Behaves like the standard Unix wc command run with no flags (returns all
    three counts). Works on text files only.

    Args:
        fs: AgentFS Filesystem instance
        path: Path to the file

    Returns:
        WcResult with line, word, and byte counts.
        If the file is binary, returns WcResult with only the byte count and a message.

    Raises:
        ErrnoException: ENOENT if file does not exist
        ErrnoException: EISDIR if path is a directory

    Example:
        >>> result = await wc(fs, '/data/app.py')
        >>> print(f"{result.lines} {result.words} {result.bytes}")
    """
    raw = await fs.read_file(path, encoding=None)
    assert isinstance(raw, bytes)
    byte_count = len(raw)

    if _is_binary(raw):
        return WcResult(bytes=byte_count, message=f"Binary file {path}, byte count only")

    content = raw.decode("utf-8")
    return WcResult(
        lines=content.count("\n"),
        words=len(content.split()),
        bytes=byte_count,
    )
