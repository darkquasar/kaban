"""Independent smoke catalog names from the checked-in full Insta snapshot."""

import json
from pathlib import Path


CATALOG_SNAPSHOT = (
    Path(__file__).resolve().parents[2]
    / "crates/kaban/tests/snapshots/mcp_snapshots__tools_list.snap"
)


def load_tool_names(snapshot_path=None):
    """Load unique names; an absent or malformed oracle is never accepted."""
    path = CATALOG_SNAPSHOT if snapshot_path is None else Path(snapshot_path)
    lines = path.read_text(encoding="utf-8").splitlines()
    if not lines or lines[0] != "---":
        raise ValueError(f"{path}: missing Insta metadata delimiter")
    try:
        body_start = lines.index("---", 1) + 1
    except ValueError as exc:
        raise ValueError(f"{path}: missing Insta body delimiter") from exc
    tools = json.loads("\n".join(lines[body_start:]))
    if not isinstance(tools, list) or not tools:
        raise ValueError(f"{path}: expected a nonempty array of tool objects")
    names = []
    for tool in tools:
        if not isinstance(tool, dict) or not isinstance(tool.get("name"), str):
            raise ValueError(f"{path}: each tool must have a string name")
        names.append(tool["name"])
    if len(names) != len(set(names)):
        raise ValueError(f"{path}: duplicate expected tool names")
    return tuple(sorted(names))


def tool_names_match(actual_names, expected_names):
    """Compare exact names without letting duplicate runtime entries disappear."""
    actual = list(actual_names)
    return (
        all(isinstance(name, str) for name in actual)
        and len(actual) == len(set(actual))
        and tuple(sorted(actual)) == expected_names
    )
