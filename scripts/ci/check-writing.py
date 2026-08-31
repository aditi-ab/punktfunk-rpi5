#!/usr/bin/env python3
"""Length and shape gates for commits, CHANGELOG.md, and Rust comments.

See docs/writing.md. If this fails: shorten. Do not add `writing-ok` unless
the extra lines are a SAFETY/lifetime trap.

  1. Each commit: `type(scope): summary`, ≤72 chars, no trailing period,
     no `and` / semicolon, body ≤ 200 words, no Co-Authored-By.
  2. Newest CHANGELOG section: ≤160 lines. Older sections are not counted.
  3. Opened `//` : fail at 6 lines. Opened `//!` / `///`: fail at 24.
  4. Metaphor / field-report / soak phrasing fails in all three.

A comment is opened if its lines are in the diff, or it sits above an item
(`fn` / `struct` / …) whose body this diff changed. Other comments in the
file are ignored.

`// SAFETY:` is exempt. `writing-ok:` on the line above the block, or on
its first line, with a reason. Vendor trees skipped. No cargo.
"""
from __future__ import annotations

import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

CHANGELOG_FAIL = 160
LINE_COMMENT_FAIL = 6
DOC_COMMENT_FAIL = 24
SUBJECT_FAIL = 72
BODY_WORDS_FAIL = 200

ROOT = Path(__file__).resolve().parents[2]

COMMIT_TYPES = (
    "feat", "fix", "docs", "refactor", "perf", "test", "chore", "ci", "security"
)
CONV = re.compile(
    rf"^({'|'.join(COMMIT_TYPES)})\(([A-Za-z0-9_./+-]+)\): (.+)$"
)
SKIP_SUBJECT = re.compile(r"^(Merge |Revert )")
ITEM_START = re.compile(
    r"^(pub(\([^)]*\))?\s+)?(async\s+)?(unsafe\s+)?"
    r"(fn|struct|enum|impl|trait|type|const|static|mod)\b"
)

# Shared across commits, changelog, comments. Keep this list the stories we
# actually shipped, not a vibe classifier.
STORY = (
    (re.compile(r"rolled dice", re.I), "metaphor"),
    (re.compile(r"\bon the floor\b", re.I), "metaphor"),
    (re.compile(r"\blied\b", re.I), "metaphor"),
    (re.compile(r"\bwore\b", re.I), "metaphor"),
    (re.compile(r"first section written", re.I), "meta"),
    (re.compile(r"Field 20\d\d"), "field report"),
    (re.compile(r"cured by reconnecting", re.I), "field report"),
    (re.compile(r"\bponytail:"), "lab nickname"),
    (re.compile(r"\b\d+\s*-?\s*minute(?:s)?\s+soak\b", re.I), "field measurement"),
)


def story_hits(text: str) -> list[str]:
    found = []
    for rx, label in STORY:
        if rx.search(text):
            found.append(label)
    return found


def newest_changelog_section(text: str) -> tuple[int, str, str]:
    """Return (line_count, heading, section_text) of the first `## v*` section."""
    lines = text.splitlines()
    starts = [i for i, line in enumerate(lines) if re.match(r"^## v\d", line)]
    if not starts:
        return 0, "", ""
    a = starts[0]
    b = starts[1] if len(starts) > 1 else len(lines)
    return b - a, lines[a], "\n".join(lines[a:b])


def newest_changelog_len(text: str) -> tuple[int, str]:
    n, heading, _ = newest_changelog_section(text)
    return n, heading


def iter_comment_blocks(lines: list[str]):
    """Yield (kind, start, end) half-open. kind is 'line' (`//`) or 'doc' (`//!`/`///`)."""
    i = 0
    n = len(lines)
    while i < n:
        stripped = lines[i].lstrip()
        if stripped.startswith("//!") or stripped.startswith("///"):
            start = i
            while i < n:
                s = lines[i].lstrip()
                if not lines[i].strip():
                    nxt = lines[i + 1].lstrip() if i + 1 < n else ""
                    if nxt.startswith("//!") or nxt.startswith("///"):
                        i += 1
                        continue
                    break
                if s.startswith("//!") or s.startswith("///"):
                    i += 1
                    continue
                break
            yield "doc", start, i
            continue
        if stripped.startswith("//"):
            start = i
            while i < n:
                s = lines[i].lstrip()
                if s.startswith("//") and not s.startswith("//!") and not s.startswith("///"):
                    i += 1
                    continue
                break
            yield "line", start, i
            continue
        i += 1


def _waived(lines: list[str], start: int) -> bool:
    first = lines[start].lstrip()
    if re.match(r"^//(/|!)?\s*SAFETY:", first):
        return True
    if "writing-ok:" in first:
        return True
    j = start - 1
    while j >= 0 and not lines[j].strip():
        j -= 1
    if j >= 0 and "writing-ok:" in lines[j]:
        return True
    return False


def file_level_doc(lines: list[str]) -> tuple[int, int] | None:
    """The leading `//!` block, skipping inner attributes and blanks before it."""
    i = 0
    n = len(lines)
    while i < n:
        s = lines[i].lstrip()
        if not lines[i].strip() or (s.startswith("#![") and not s.startswith("//!")):
            i += 1
            continue
        break
    if i < n and lines[i].lstrip().startswith("//!"):
        for kind, start, end in iter_comment_blocks(lines[i:]):
            if kind == "doc" and start == 0:
                return i, i + (end - start)
            break
    return None


def comment_above_item(lines: list[str], item_idx: int) -> tuple[int, int] | None:
    """Comment block immediately above an item, allowing attrs and blanks between."""
    j = item_idx - 1
    while j >= 0 and (not lines[j].strip() or lines[j].lstrip().startswith("#[")):
        j -= 1
    if j < 0:
        return None
    s = lines[j].lstrip()
    if not (s.startswith("//") or s.startswith("///") or s.startswith("//!")):
        return None
    for kind, start, end in iter_comment_blocks(lines):
        if start <= j < end:
            return start, end
    return None


def opened_comment_spans(
    lines: list[str], touched: set[int] | None
) -> set[tuple[int, int]]:
    """Start/end of comment blocks this diff opened. None = every block."""
    blocks = list(iter_comment_blocks(lines))
    if touched is None:
        return {(start, end) for _, start, end in blocks}
    opened: set[tuple[int, int]] = set()
    header = file_level_doc(lines)
    for kind, start, end in blocks:
        span = set(range(start + 1, end + 1))
        if not span.isdisjoint(touched):
            opened.add((start, end))
    for lineno in touched:
        i = lineno - 1
        if i < 0 or i >= len(lines):
            continue
        k = i
        found = None
        while k >= 0:
            if ITEM_START.match(lines[k].lstrip()) and not lines[k].lstrip().startswith("//"):
                found = k
                break
            k -= 1
        if found is None:
            continue
        attached = comment_above_item(lines, found)
        if attached:
            opened.add(attached)
    # A module header is opened only when the header itself moved, not when
    # some function in the file did. New files add every line, so they count.
    if header is not None:
        hspan = set(range(header[0] + 1, header[1] + 1))
        if hspan.isdisjoint(touched):
            opened.discard(header)
    return opened


def check_blocks(
    path: str,
    lines: list[str],
    touched: set[int] | None,
    file_touched: bool = True,
) -> list[str]:
    """Line numbers in `touched` are 1-based. None means every block.

    `file_touched` is accepted for call-site compatibility; opening is decided
    per comment, not per file.
    """
    del file_touched
    errors = []
    opened = opened_comment_spans(lines, touched)
    kinds = {(start, end): kind for kind, start, end in iter_comment_blocks(lines)}
    for start, end in sorted(opened):
        if _waived(lines, start):
            continue
        kind = kinds.get((start, end), "line")
        length = end - start
        limit = DOC_COMMENT_FAIL if kind == "doc" else LINE_COMMENT_FAIL
        lead = lines[start].strip()[:80]
        which = "//! / ///" if kind == "doc" else "//"
        if length >= limit:
            errors.append(
                f"{path}:{start + 1}: {which} block is {length} lines "
                f"(fail at {limit}). Rewrite what you opened: {lead}"
            )
        text = "\n".join(lines[start:end])
        for label in story_hits(text):
            errors.append(
                f"{path}:{start + 1}: {which} block is a {label}. "
                f"Put the incident on the PR. Rewrite what you opened: {lead}"
            )
    return errors


def check_commit(subject: str, body: str, sha: str = "") -> list[str]:
    loc = f"commit {sha[:12]} " if sha else "commit "
    errors = []
    if SKIP_SUBJECT.match(subject):
        return errors
    if len(subject) > SUBJECT_FAIL:
        errors.append(
            f"{loc}subject is {len(subject)} chars (fail at {SUBJECT_FAIL}): {subject!r}"
        )
    if subject.endswith("."):
        errors.append(f"{loc}subject has a trailing period: {subject!r}")
    m = CONV.match(subject)
    if not m:
        errors.append(
            f"{loc}subject must be `type(scope): summary` "
            f"(types: {', '.join(COMMIT_TYPES)}): {subject!r}"
        )
    else:
        summary = m.group(3)
        if summary.startswith("The "):
            errors.append(
                f"{loc}subject starts with `The`. Use type(scope) and an imperative "
                f"verb: {subject!r}"
            )
        if " and " in summary or ";" in summary:
            errors.append(
                f"{loc}subject joins two changes (`and` / `;`). Split the commit "
                f"or name one theme: {subject!r}"
            )
    if re.search(r"^Co-Authored-By:", body, re.M | re.I):
        errors.append(f"{loc}has Co-Authored-By. Credit a co-author in prose.")
    words = len(body.split())
    if words > BODY_WORDS_FAIL:
        errors.append(
            f"{loc}body is {words} words (fail at {BODY_WORDS_FAIL}). "
            "Investigation goes on the PR."
        )
    for label in story_hits(subject + "\n" + body):
        errors.append(
            f"{loc}is a {label}. Field logs and metaphors go on the PR, not the commit."
        )
    return errors


def parse_diff_changed_lines(diff: str) -> dict[str, set[int]]:
    files: dict[str, set[int]] = defaultdict(set)
    path = None
    new_line = 0
    for line in diff.splitlines():
        if line.startswith("diff --git "):
            path = None
            m = re.search(r" b/(.+)$", line)
            if m:
                path = m.group(1)
            continue
        if line.startswith("+++ "):
            rest = line[4:]
            if rest.startswith("b/"):
                path = rest[2:]
            continue
        if line.startswith("@@"):
            m = re.search(r"\+(\d+)(?:,(\d+))?", line)
            if not m:
                continue
            new_line = int(m.group(1))
            continue
        if line.startswith("+") and not line.startswith("+++"):
            if path:
                files[path].add(new_line)
            new_line += 1
            continue
        if line.startswith("-") and not line.startswith("---"):
            continue
        if line.startswith("\\"):
            continue
    return files


def git_merge_base() -> str | None:
    env_base = os.environ.get("WRITING_BASE") or os.environ.get("GITHUB_BASE_REF")
    candidates = []
    if env_base:
        candidates.append(env_base)
        candidates.append(f"origin/{env_base}")
    candidates.extend(["main", "origin/main", "unom/main"])
    for ref in candidates:
        try:
            base = subprocess.check_output(
                ["git", "merge-base", "HEAD", ref],
                cwd=ROOT,
                text=True,
                stderr=subprocess.DEVNULL,
            ).strip()
            if base:
                return base
        except subprocess.CalledProcessError:
            continue
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD^"],
            cwd=ROOT,
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except subprocess.CalledProcessError:
        return None


def changed_rs_lines(base: str) -> dict[str, set[int]]:
    diff = subprocess.check_output(
        ["git", "diff", "-U0", f"{base}...HEAD", "--", "*.rs", ":!**/vendor/**"],
        cwd=ROOT,
        text=True,
    )
    return parse_diff_changed_lines(diff)


def commits_since(base: str) -> list[tuple[str, str, str]]:
    raw = subprocess.check_output(
        ["git", "log", "-z", "--format=%H%x1f%s%x1f%b", f"{base}..HEAD"],
        cwd=ROOT,
        text=True,
    )
    out = []
    for rec in raw.split("\0"):
        if not rec.strip():
            continue
        parts = rec.split("\x1f", 2)
        if len(parts) != 3:
            continue
        sha, subject, body = parts
        out.append((sha.strip(), subject.strip(), body.strip()))
    return out


def check_repo() -> list[str]:
    errors: list[str] = []
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    n, heading, section = newest_changelog_section(changelog)
    if n >= CHANGELOG_FAIL:
        errors.append(
            f"CHANGELOG.md: newest section {heading!r} is {n} lines "
            f"(fail at {CHANGELOG_FAIL}). Shorten; do not edit older sections."
        )
    for label in story_hits(section):
        errors.append(
            f"CHANGELOG.md: newest section {heading!r} is a {label}. "
            "Two sentences per bullet; stories go on the PR."
        )
    base = git_merge_base()
    if base:
        for sha, subject, body in commits_since(base):
            errors.extend(check_commit(subject, body, sha=sha))
    changed: dict[str, set[int]] = defaultdict(set)
    if base:
        for rel, lines_touched in changed_rs_lines(base).items():
            changed[rel].update(lines_touched)
    try:
        wt = subprocess.check_output(
            ["git", "diff", "-U0", "HEAD", "--", "*.rs", ":!**/vendor/**"],
            cwd=ROOT,
            text=True,
        )
        for rel, lines_touched in parse_diff_changed_lines(wt).items():
            changed[rel].update(lines_touched)
    except subprocess.CalledProcessError:
        pass
    for rel, lines_touched in sorted(changed.items()):
        if "/vendor/" in rel.split("/"):
            continue
        path = ROOT / rel
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        errors.extend(check_blocks(rel, text.splitlines(), lines_touched))
    return errors


def self_test() -> int:
    fails = 0

    def expect(label: str, cond: bool) -> None:
        nonlocal fails
        if not cond:
            print(f"FAIL: {label}", file=sys.stderr)
            fails += 1

    short = "## v1.0.0\n\n### Fixed\n- **Foo.** Bar.\n\n## v0.9.0\n"
    n, h = newest_changelog_len(short)
    expect("short changelog counted", n < CHANGELOG_FAIL and h == "## v1.0.0")

    long = "## v1.0.0\n" + ("x\n" * CHANGELOG_FAIL) + "## v0.9.0\n"
    n, _ = newest_changelog_len(long)
    expect("long changelog counted", n >= CHANGELOG_FAIL)

    story_cl, _, sec = newest_changelog_section(
        "## v1.0.0\n- clients rolled dice\n\n## v0.9.0\n"
    )
    expect("changelog story", "metaphor" in story_hits(sec) and story_cl == 3)

    five = ["// a"] * 5 + ["fn x() {}"]
    expect("five // pass", check_blocks("t.rs", five, {1, 2, 3, 4, 5}) == [])

    six = ["// a"] * 6 + ["fn x() {}"]
    err = check_blocks("t.rs", six, {1, 2, 3, 4, 5, 6})
    expect("six // fail", len(err) >= 1 and any("fail at 6" in e for e in err))

    untouched = check_blocks("t.rs", six, {8}, file_touched=False)
    expect("untouched six // pass", untouched == [])

    # Touching the function body opens the comment above it.
    leftover = ["// leftover field report"] * 6 + ["fn foo() {", "    let x = 1;", "}"]
    err = check_blocks("t.rs", leftover, {8})
    expect(
        "opened function comment",
        any("fail at 6" in e and "Rewrite what you opened" in e for e in err),
    )

    short_above = ["// why 250 ms"] + ["fn foo() {", "    let x = 1;", "}"]
    expect("short attached comment", check_blocks("t.rs", short_above, {3}) == [])

    safety = ["// SAFETY: the pointer is aligned"] + ["// still"] * 6 + ["unsafe {}"]
    expect("SAFETY exempt", check_blocks("t.rs", safety, {1}) == [])

    waived = ["// writing-ok: generation vs session"] + ["// x"] * 6 + ["fn y() {}"]
    expect("writing-ok above", check_blocks("t.rs", waived, {2}) == [])

    field = ["// Field 2026-08-28, iPad Pro: froze", "fn foo() {", "    x();", "}"]
    err = check_blocks("t.rs", field, {3})
    expect("field report opened", any("field report" in e for e in err))

    doc_ok = ["//! m"] * 23 + ["pub fn z() {}"]
    expect("23 //! pass", check_blocks("t.rs", doc_ok, None) == [])

    doc_bad = ["//! m"] * 24 + ["pub fn z() {}"]
    err = check_blocks("t.rs", doc_bad, None)
    expect("24 //! fail", any("fail at 24" in e for e in err))

    # A long module header is not opened by editing a function 30 lines down.
    err = check_blocks("t.rs", doc_bad, {30})
    expect("body edit skips header", err == [])

    err = check_blocks("t.rs", doc_bad, {1})
    expect("header edit is opened", any("fail at 24" in e for e in err))

    header_and_fn = ["//! m"] * 24 + ["pub fn z() {", "    let x = 1;", "}"]
    err = check_blocks("t.rs", header_and_fn, {26})
    expect("fn body does not open //!", err == [])

    diff = """diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,0 +11,2 @@
+// one
+// two
"""
    parsed = parse_diff_changed_lines(diff)
    expect("diff lines", parsed.get("src/lib.rs") == {11, 12})

    good = "fix(host/hyprland): keep topology restore across pipeline retries"
    expect("good commit", check_commit(good, "The restore was dropped.\n") == [])

    plot = "The retry loop stops eating the restore that re-lights the desk"
    err = check_commit(plot, "")
    expect("plot subject", any("type(scope)" in e for e in err))

    err = check_commit("fix(host): keep the restore.", "")
    expect("trailing period", any("trailing period" in e for e in err))

    err = check_commit("fix(host): keep foo and skip bar", "")
    expect("and in subject", any("`and`" in e for e in err))

    long_subj = "fix(host): " + "x" * SUBJECT_FAIL
    err = check_commit(long_subj, "")
    expect("long subject", any("chars" in e for e in err))

    err = check_commit("fix(host): keep it", "Co-Authored-By: x <x@y>")
    expect("trailer", any("Co-Authored-By" in e for e in err))

    err = check_commit("fix(host): keep it", " ".join(["word"] * (BODY_WORDS_FAIL + 1)))
    expect("long body", any("words" in e for e in err))

    conv_the = "fix(host): The retry loop stops eating the restore"
    err = check_commit(conv_the, "")
    expect("The-subject", any("starts with `The`" in e for e in err))

    if fails:
        print(f"{fails} self-test failure(s)", file=sys.stderr)
        return 1
    print("check-writing self-test ok")
    return 0


def main(argv: list[str]) -> int:
    if argv[1:] == ["--self-test"]:
        return self_test()
    os.chdir(ROOT)
    errors = check_repo()
    if errors:
        for e in errors:
            print(f"::error::{e}")
        print(
            "docs/writing.md: shorten the commit, the changelog bullet, or the "
            "comment you opened. Do not add writing-ok unless the extra lines "
            "are a SAFETY/lifetime trap.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
