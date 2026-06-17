#!/usr/bin/env python3
"""Write a copy of a Cargo.lock with the microsoft/windows-rs git packages removed.

The punktfunk workspace lockfile includes `punktfunk-client-windows`' git dependencies — the
whole microsoft/windows-rs tree (windows-reactor + ~13 `windows-*` crates, all pinned to one
git rev). The flatpak builds ONLY the Linux client (`-p punktfunk-client-linux`), which never
references them, but `flatpak-cargo-generator.py` walks the whole lock and emits a `type: git`
source for windows-rs; `flatpak-builder` then FULL-clones that multi-GB repo at "Downloading
sources" → "No space left on device", failing the build.

Stripping those packages from the lock the generator sees is safe: the committed Cargo.lock is
left untouched for the `--locked` build, and `cargo --offline` only needs vendored sources for
the crates it actually compiles (none of the windows-rs crates are in the Linux client's
dependency closure).

Dependency-free (no tomlkit) so it also runs on the Steam Deck's stock python. Matches on the
`source =` line specifically, so a crate that merely *lists* a windows-rs dependency is kept.

Usage: prune-windows-lock.py <in Cargo.lock> <out lock>
"""
import sys

WINDOWS_RS = "github.com/microsoft/windows-rs"


def is_windows_rs(block: str) -> bool:
    for line in block.splitlines():
        s = line.strip()
        if s.startswith("source = ") and WINDOWS_RS in s:
            return True
    return False


def main() -> None:
    src, dst = sys.argv[1], sys.argv[2]
    text = open(src).read()
    # Cargo.lock (v3+) is a header followed by `[[package]]` blocks, no trailing [metadata].
    parts = text.split("\n[[package]]\n")
    header = parts[0]
    kept, removed = [], 0
    for block in parts[1:]:
        if is_windows_rs(block):
            removed += 1
        else:
            kept.append(block)
    out = header + "".join("\n[[package]]\n" + b for b in kept)
    open(dst, "w").write(out)
    print(f"prune-windows-lock: removed {removed} windows-rs git crates ({src} -> {dst})")


if __name__ == "__main__":
    main()
