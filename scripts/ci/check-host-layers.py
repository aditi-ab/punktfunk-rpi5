#!/usr/bin/env python3
"""A domain module of punktfunk-host never names a module above it.

`mgmt` is the HTTP layer: it calls `game`, `library` and `plugin_host`, and none of them calls it.
`game` sits above the other two the same way, and `library` above `plugin_host`: the CLI and
the store route hand `plugin_host` the function that deletes a provider's titles. main.rs re-exports some submodules flat
(`use game::{gamelease, …}`), so a denied module is denied under those names too. Comment lines
are skipped: a doc link is not an import.

Usage: python3 scripts/ci/check-host-layers.py [--self-test]
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SRC = os.path.join(ROOT, "crates", "host", "punktfunk-host", "src")

# A module on the left names none of the modules on its row.
DENY = {
    "game": ["mgmt"],
    "library": ["mgmt", "game"],
    "plugin_host": ["mgmt", "game", "library"],
}


def aliases(main_rs, module):
    """`module`, and the submodules main.rs re-exports flat from it."""
    flat = re.search(rf"^use {module}::\{{([^}}]*)\}};", main_rs, re.M)
    return {module, *re.findall(r"\w+", flat.group(1) if flat else "")}


def code(text):
    return "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("//"))


def named(text, names):
    """The `names` this source reaches through `crate::` or `super::`."""
    alt = "|".join(sorted(names))
    hits = set(re.findall(rf"\b(?:crate|super)::({alt})\b", text))
    for group in re.findall(r"\bcrate::\{([^;]*)\}", text):  # use crate::{a, b::c};
        hits |= set(re.findall(rf"(?:^|[{{,])\s*({alt})\b", group))
    return hits


def sources(module):
    yield os.path.join(SRC, module + ".rs")
    for dirpath, _, files in os.walk(os.path.join(SRC, module)):
        for f in sorted(files):
            if f.endswith(".rs"):
                yield os.path.join(dirpath, f)


def violations():
    with open(os.path.join(SRC, "main.rs"), encoding="utf-8") as f:
        main_rs = f.read()
    out = []
    for module, above in DENY.items():
        names = set().union(*(aliases(main_rs, m) for m in above))
        for path in sources(module):
            with open(path, encoding="utf-8") as f:
                hits = named(code(f.read()), names)
            for hit in sorted(hits):
                rel = os.path.relpath(path, ROOT)
                out.append(f"{rel}: `{module}` names `{hit}`, which sits above it")
    return out


def self_test():
    main_rs = "mod game;\nuse game::{\n    gamelease, holds,\n};\nmod mgmt;\n"
    assert aliases(main_rs, "game") == {"game", "gamelease", "holds"}, aliases(main_rs, "game")
    assert aliases(main_rs, "mgmt") == {"mgmt"}
    deny = aliases(main_rs, "game") | {"mgmt", "library"}
    for src, want in [
        ("use crate::mgmt::plugins;", {"mgmt"}),
        ("let g = crate::gamelease::GameRef::default();", {"gamelease"}),
        ("use crate::{\n    events,\n    mgmt::shared,\n};", {"mgmt"}),
        ("use super::super::mgmt;", {"mgmt"}),
        ("crate::library::delete_provider(&id);", {"library"}),
        ("/// See [`crate::mgmt::store`].\n    // crate::holds::STAGES", set()),
        ("use crate::mgmt_token::TRAY_FILE;\ncrate::webtransport::mgmt::serve();", set()),
    ]:
        assert named(code(src), deny) == want, (src, named(code(src), deny))
    print("check-host-layers self-test ok")


def main(argv):
    if "--self-test" in argv:
        return self_test()
    bad = violations()
    for line in bad:
        print(f"::error::{line}")
    if bad:
        sys.exit(1)
    print(f"check-host-layers: {', '.join(DENY)} name nothing above them")


if __name__ == "__main__":
    main(sys.argv[1:])
