#!/usr/bin/env python3
"""Generate THIRD-PARTY-NOTICES.txt for the Rust workspace.

Offline, dependency-free attribution generator. It reads `cargo metadata`, then for every
third-party crate (everything that is NOT a first-party workspace member) it pulls the crate's
*actual* LICENSE/COPYING/NOTICE text out of the local cargo registry cache (or the in-tree
vendored source for path deps), deduplicates identical license texts, and emits a single
notices file: a per-crate manifest followed by the verbatim license texts.

This satisfies the binary-distribution attribution duty for the permissive (MIT/BSD/ISC/Zlib/
Apache/Unicode/etc.) crates linked into shipped punktfunk artifacts. `cargo about` (see
about.toml) produces an equivalent, network-augmented result in CI; this is the dependency-free
fallback that also runs locally and is committed as a baseline.

Usage:  python3 scripts/gen-third-party-notices.py [--out THIRD-PARTY-NOTICES.txt]
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys

LICENSE_GLOBS = ("license", "licence", "copying", "notice", "unlicense", "copyright")


def find_license_files(pkg_dir):
    out = []
    try:
        names = sorted(os.listdir(pkg_dir))
    except OSError:
        return out
    for n in names:
        low = n.lower()
        if any(low == g or low.startswith(g + ".") or low.startswith(g + "-") or g in low for g in LICENSE_GLOBS):
            p = os.path.join(pkg_dir, n)
            if os.path.isfile(p):
                try:
                    with open(p, "r", encoding="utf-8", errors="replace") as f:
                        txt = f.read().strip()
                    if txt:
                        out.append((n, txt))
                except OSError:
                    pass
    return out


# Third-party source trees VENDORED inside first-party workspace crates — the
# workspace-member skip in main() hides them from `cargo metadata`, so they are listed
# here explicitly: (label, license file relative to the repo root, source URL).
VENDORED_TREES = [
    ("pyrowave (vendored, crates/pyrowave-sys)",
     "crates/pyrowave-sys/vendor/pyrowave/LICENSE",
     "https://github.com/Themaister/pyrowave"),
    ("Granite subset (vendored, crates/pyrowave-sys)",
     "crates/pyrowave-sys/vendor/pyrowave/Granite/LICENSE",
     "https://github.com/Themaister/Granite"),
    ("volk (vendored, crates/pyrowave-sys)",
     "crates/pyrowave-sys/vendor/pyrowave/Granite/third_party/volk/LICENSE.md",
     "https://github.com/zeux/volk"),
    ("Vulkan-Headers (vendored, crates/pyrowave-sys)",
     "crates/pyrowave-sys/vendor/pyrowave/Granite/third_party/khronos/vulkan-headers/LICENSE.md",
     "https://github.com/KhronosGroup/Vulkan-Headers"),
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="THIRD-PARTY-NOTICES.txt")
    ap.add_argument("--manifest", default="Cargo.toml")
    args = ap.parse_args()

    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--offline", "--manifest-path", args.manifest],
        text=True))
    ws_members = set(meta.get("workspace_members", []))

    pkgs = []
    for p in meta["packages"]:
        if p["id"] in ws_members:
            continue  # first-party (covered by the root LICENSE-MIT / LICENSE-APACHE)
        pkgs.append(p)
    pkgs.sort(key=lambda p: (p["name"].lower(), p["version"]))

    # Group license texts: text-hash -> {text, name, crates[]}
    texts = {}
    no_text = []
    for p in pkgs:
        pkg_dir = os.path.dirname(p["manifest_path"])
        files = find_license_files(pkg_dir)
        label = f'{p["name"]} {p["version"]}'
        if not files:
            no_text.append(p)
            continue
        for fname, txt in files:
            h = hashlib.sha256(txt.encode("utf-8", "replace")).hexdigest()
            ent = texts.setdefault(h, {"text": txt, "filename": fname, "crates": set()})
            ent["crates"].add(label)

    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    vendored = []
    for label, lic_path, url in VENDORED_TREES:
        full = os.path.join(repo_root, lic_path)
        try:
            with open(full, encoding="utf-8", errors="replace") as f:
                txt = f.read().strip()
        except OSError:
            print(f"WARNING: vendored license missing: {lic_path}", file=sys.stderr)
            continue
        vendored.append((label, url))
        h = hashlib.sha256(txt.encode("utf-8", "replace")).hexdigest()
        ent = texts.setdefault(
            h, {"text": txt, "filename": os.path.basename(lic_path), "crates": set()}
        )
        ent["crates"].add(label)

    lines = []
    w = lines.append
    w("THIRD-PARTY SOFTWARE NOTICES")
    w("=" * 76)
    w("")
    w("punktfunk (https://git.unom.io/unom/punktfunk) is licensed under MIT OR Apache-2.0.")
    w("The binaries it ships statically/dynamically link the third-party Rust crates listed")
    w("below. Each is distributed under its own permissive license; the full license texts")
    w("follow the manifest. This file is generated by scripts/gen-third-party-notices.py")
    w("(or `cargo about`, see about.toml) — do not edit by hand.")
    w("")
    w(f"Total third-party crates: {len(pkgs)}")
    w("")
    if vendored:
        w("-" * 76)
        w("VENDORED THIRD-PARTY SOURCE (inside first-party crates)")
        w("-" * 76)
        for label, url in vendored:
            w(f"  {label} — {url}")
        w("")
    w("-" * 76)
    w("MANIFEST (crate version — SPDX license — source)")
    w("-" * 76)
    for p in pkgs:
        lic = p.get("license") or (("file: " + p["license_file"]) if p.get("license_file") else "UNKNOWN")
        repo = p.get("repository") or ""
        w(f'  {p["name"]} {p["version"]} — {lic}' + (f' — {repo}' if repo else ""))
    w("")

    if no_text:
        w("-" * 76)
        w("Crates whose package did not embed a license file (SPDX + source only)")
        w("-" * 76)
        for p in no_text:
            lic = p.get("license") or "UNKNOWN"
            repo = p.get("repository") or ""
            w(f'  {p["name"]} {p["version"]} — {lic}' + (f' — {repo}' if repo else ""))
        w("")

    w("=" * 76)
    w("FULL LICENSE TEXTS (deduplicated)")
    w("=" * 76)
    # Stable order: by first crate name covered.
    for h, ent in sorted(texts.items(), key=lambda kv: sorted(kv[1]["crates"])[0].lower()):
        crates = ", ".join(sorted(ent["crates"]))
        w("")
        w("-" * 76)
        w(f"The following license ({ent['filename']}) applies to: {crates}")
        w("-" * 76)
        w(ent["text"])
        w("")

    text = "\n".join(lines) + "\n"
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(text)
    print(f"wrote {args.out}: {len(pkgs)} crates, {len(texts)} distinct license texts, "
          f"{len(no_text)} without embedded text", file=sys.stderr)


if __name__ == "__main__":
    main()
