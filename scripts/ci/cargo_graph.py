"""Dependency closure over `cargo metadata` output, shared by the notices generator and
check-workflow-paths.py."""


def closure(meta, roots):
    """Package ids reachable from the packages named `roots` through the resolve graph.

    It follows whatever graph `meta` holds: the whole feature-unified resolve by default, or one
    target's slice when the caller ran `cargo metadata --filter-platform`. It never pulls in
    crates reachable only from other workspace members.
    """
    by_name = {}
    for p in meta["packages"]:
        by_name.setdefault(p["name"], p["id"])
    nodes = {n["id"]: n for n in meta.get("resolve", {}).get("nodes", [])}
    seen, stack = set(), []
    for r in roots:
        pid = by_name.get(r)
        if pid is None:
            raise SystemExit(f"no package named {r!r} in this workspace")
        stack.append(pid)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        stack.extend(nodes.get(pid, {}).get("dependencies", []))
    return seen
