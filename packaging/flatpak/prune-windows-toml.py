#!/usr/bin/env python3
"""Strip microsoft/windows-rs git dependency entries from a Cargo.toml, in place.

The sibling of prune-windows-lock.py, for the OTHER place the un-vendored windows-rs git
source leaks into the flatpak's offline build: `crates/pf-client-core` declares
`windows = { git = ... }` under `[target.'cfg(windows)'.dependencies]` (the D3D11VA decode
backend). The dependency is cfg-gated and never COMPILES on Linux, but `cargo --offline`
still needs every declared dependency's source available just to build the unit graph — and
windows-rs is deliberately not vendored into cargo-sources.json (flatpak-builder would
full-clone the multi-GB repo; see prune-windows-lock.py). Removing the entry from the
sandbox copy of the manifest is safe for a Linux-only build: nothing behind cfg(windows)
is compiled, so nothing misses the crate.

Removes any top-level `name = { ... git = "...github.com/microsoft/windows-rs..." ... }`
entry, single- or multi-line (pf-client-core's spans a features array; the entry ends at
the first line that closes back to depth 0). Registry deps in the same table (wasapi,
sdl3) are kept — they vendor normally.

Dependency-free (no tomlkit) so it also runs inside the flatpak build sandbox and on the
Steam Deck's stock python.

Usage: prune-windows-toml.py <Cargo.toml> [<Cargo.toml> ...]
"""

import sys

WINDOWS_RS = "github.com/microsoft/windows-rs"


def prune(text: str) -> tuple[str, int]:
    lines = text.splitlines(keepends=True)
    kept: list[str] = []
    removed = 0
    i = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        # A dependency entry opening an inline table that pins the windows-rs git repo.
        # Single-line entries contain the URL and balance their braces on the same line;
        # multi-line entries (features arrays) run until the braces balance again.
        if "=" in stripped and "{" in stripped and not stripped.startswith("#"):
            depth = stripped.count("{") - stripped.count("}")
            entry = [line]
            j = i + 1
            while depth > 0 and j < len(lines):
                s = lines[j]
                depth += s.count("{") - s.count("}")
                entry.append(s)
                j += 1
            if any(WINDOWS_RS in e for e in entry):
                removed += 1
                i = j
                continue
        kept.append(line)
        i += 1
    return "".join(kept), removed


def main() -> None:
    for path in sys.argv[1:]:
        text = open(path).read()
        out, removed = prune(text)
        if removed == 0:
            sys.exit(f"{path}: no windows-rs git entry found — already pruned, or the "
                     "dependency moved (update this script's callers)")
        open(path, "w").write(out)
        print(f"{path}: removed {removed} windows-rs git dependenc{'y' if removed == 1 else 'ies'}")


if __name__ == "__main__":
    main()
