#!/usr/bin/env python3
"""Block direct edits to Beads-generated workstream mirrors."""

from __future__ import annotations

import json
import re
import sys
from typing import Any, Iterable


GENERATED_PATH_RE = re.compile(
    r"docs/workstreams/(?:.+/tracking/[^/\s]+\.md|(?:status|ideas|backlog)\.md)"
)


def iter_strings(value: Any) -> Iterable[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from iter_strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from iter_strings(item)


def main() -> int:
    raw = sys.stdin.read()
    if not raw.strip():
        return 0

    try:
        payload = json.loads(raw)
    except json.JSONDecodeError:
        return 0

    tool_input = payload.get("tool_input", {})
    candidates = list(iter_strings(tool_input))

    for candidate in candidates:
        match = GENERATED_PATH_RE.search(candidate)
        if match:
            print(
                "BLOCKED: "
                f"'{match.group(0)}' is a bd-generated workstream mirror. "
                "Update bd, then regenerate it with the project renderer. "
                "Do not hand-edit generated mirrors.",
                file=sys.stderr,
            )
            return 2

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
