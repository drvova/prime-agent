"""Exact single-occurrence string replacement for existing files."""

from __future__ import annotations

from pathlib import Path


async def run(path: str, old_str: str, new_str: str) -> str:
    """Replace a unique string in a file.

    ``old_str`` must appear exactly once in the file at ``path``; that match is
    replaced with ``new_str`` and the file is written back in place. Prefer this
    over rewriting a whole file for targeted edits.

    Args:
        path: File to edit, relative to the working directory, absolute, or
            `~`-prefixed (the leading `~`/`~user` is expanded to the home dir).
        old_str: Exact text to find. Must occur exactly once in the file.
        new_str: Replacement text.

    Returns:
        A short confirmation message.

    Raises:
        FileNotFoundError: If ``path`` does not exist.
        ValueError: If ``old_str`` is absent (the message says where it
            diverges from the file) or matches more than once.
    """
    filepath = Path(path).expanduser()
    if not filepath.exists():
        raise FileNotFoundError(f"{path} not found")
    content = filepath.read_text(encoding="utf-8")
    count = content.count(old_str)
    if count == 0:
        raise ValueError(_miss_diagnostic(path, content, old_str))
    if count > 1:
        raise ValueError(
            f"found {count} occurrences in {path}, need exactly 1 — "
            "widen the snippet to make it unique"
        )
    match_index = content.index(old_str)
    start_line = content.count("\n", 0, match_index) + 1
    filepath.write_text(content.replace(old_str, new_str, 1), encoding="utf-8")
    resolved_path = str(filepath.resolve())
    _emit_diff(resolved_path, old_str, new_str, start_line)
    return f"Edited {resolved_path}"


# Characters of the file and of old_str shown at the divergence point.
_SPLIT_CONTEXT = 60


def _miss_diagnostic(path: str, content: str, old_str: str) -> str:
    """Say where old_str stops matching, so the next edit can fix it without
    another read: the longest leading piece of old_str the file contains (at
    its first occurrence), a bounded view of both sides at the split, and a
    hint when the split is a line-ending or whitespace difference."""
    # Containment is monotonic in the prefix length, so binary-search the
    # longest prefix the file contains (the whole old_str is known absent).
    low, high = 0, len(old_str) - 1
    while low < high:
        middle = (low + high + 1) // 2
        if old_str[:middle] in content:
            low = middle
        else:
            high = middle - 1
    if low == 0:
        return f"string not found in {path}: the file does not contain even the first character of old_str"
    point = content.index(old_str[:low]) + low
    line = content.count("\n", 0, point) + 1
    column = point - (content.rfind("\n", 0, point) + 1) + 1
    file_next = content[point : point + _SPLIT_CONTEXT]
    old_next = old_str[low : low + _SPLIT_CONTEXT]
    message = (
        f"string not found in {path}: old_str matches its first {low} of {len(old_str)} characters "
        f"and diverges at line {line}, column {column}:\n"
        f"  file continues    {file_next!r}\n"
        f"  old_str continues {old_next!r}"
    )
    # read_text() normalizes line endings, so a CRLF difference always shows
    # as old_str's \r against the file's \n.
    if old_next.startswith("\r") and file_next.startswith("\n"):
        message += "\nhint: old_str has CRLF (\\r\\n) line endings; write them as \\n (the file reads with \\n)"
    elif file_next[:1].isspace() or old_next[:1].isspace():
        message += "\nhint: whitespace differs at the split (indentation, tabs vs spaces, or trailing spaces)"
    return message


# Keep in sync with DIFF_DISPLAY_MIME in src/core/kernel/index.ts.
_DIFF_DISPLAY_MIME = "application/vnd.prime-agent.diff+json"


def _emit_diff(path: str, old_str: str, new_str: str, start_line: int) -> None:
    """Stream a diff to the host as a display event; best-effort outside the kernel."""
    try:
        from rlm import emit

        emit(
            {
                _DIFF_DISPLAY_MIME: {
                    "path": path,
                    "old_str": old_str,
                    "new_str": new_str,
                    "start_line": start_line,
                },
                "text/plain": f"Edited {path}",
            }
        )
    except Exception:
        pass
