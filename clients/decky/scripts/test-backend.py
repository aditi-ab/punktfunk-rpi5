#!/usr/bin/env python3
"""Unit checks for main.py's pure helpers — stdlib only, no Decky runtime needed.

Stubs the ``decky`` module (main.py imports it at module level), then asserts the argv
shapes, the exit-code mapping and the Steam VDF editor against fixtures.

Needs Python >= 3.10 for `X | None` annotations — macOS ships 3.9, so run it explicitly:

    python3.13 clients/decky/scripts/test-backend.py
"""

import sys
import types
from pathlib import Path

# ---- stub the decky module before importing main.py ------------------------------------
decky = types.ModuleType("decky")
decky.DECKY_USER_HOME = "/tmp/pf-test-home"
decky.DECKY_PLUGIN_DIR = "/tmp/pf-test-plugin"


class _Log:
    def __getattr__(self, _name):
        return lambda *a, **k: None


decky.logger = _Log()
sys.modules["decky"] = decky

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import main  # noqa: E402  (the plugin backend)

failures = 0


def check(name: str, cond: bool):
    global failures
    print(("ok  " if cond else "FAIL") + " " + name)
    if not cond:
        failures += 1


# ---- _cli_argv: the flatpak app id must stay LAST ---------------------------------------
#
# `flatpak run --command=X <app-id> ARGS` — everything after the app id is the APP's argv, so
# an app id that drifts left silently turns our flags into the client's. This is the shape the
# deleted _session_argv used and the one thing about it that is easy to get wrong.
main._client_argv = lambda: ["/usr/bin/flatpak", "run", "--arch=x86_64", "io.unom.Punktfunk"]
main._flatpak = lambda: "/usr/bin/flatpak"
check(
    "cli argv: flatpak form, app id last",
    main._cli_argv()
    == [
        "/usr/bin/flatpak",
        "run",
        "--arch=x86_64",
        "--command=punktfunk",
        "io.unom.Punktfunk",
    ],
)

# A native install: the CLI is the client binary's sibling. Absent => no CLI at all, which the
# caller must see as "unavailable" rather than as an empty result.
#
# The fixture dir is torn down FIRST, not just created: leaving the sibling behind made the
# "absent" assertion below pass only on the first run of the day and fail on every rerun.
import shutil  # noqa: E402

shutil.rmtree("/tmp/pf-test-native", ignore_errors=True)
tmp = Path("/tmp/pf-test-native/bin")
tmp.mkdir(parents=True, exist_ok=True)
(tmp / "punktfunk-client").write_text("")
main._client_argv = lambda: [str(tmp / "punktfunk-client")]
check("cli argv: native without a sibling CLI is None", main._cli_argv() is None)
(tmp / "punktfunk").write_text("")
check("cli argv: native sibling found", main._cli_argv() == [str(tmp / "punktfunk")])

# ---- _cli_error: the CLI's exit-code contract -------------------------------------------
#
# Exit 5 + `unknown command` is how a client too old for a verb announces itself — the ONE
# signature the panel turns into "update the client" plus the button that fixes it. Getting it
# wrong makes an out-of-date client look like a broken plugin.
check(
    "err: unknown verb => client-outdated",
    main._cli_error(5, 'unknown command "discover"\n\npunktfunk — the Punktfunk client')
    == "client-outdated",
)
check(
    "err: exit 5 without that phrase is NOT outdated",
    main._cli_error(5, 'no saved host matches "desk"') == "unresolved",
)
check("err: connect failed", main._cli_error(2, "unreachable 10.0.0.1:9777") == "unreachable")
check("err: trust rejected", main._cli_error(3, "wrong PIN") == "refused")
check("err: needs a person", main._cli_error(6, "pair it first") == "needs-pairing")
check("err: nothing ran", main._cli_error(-1, "") == "client-unavailable")
check("err: unmapped code falls back", main._cli_error(4, "renderer") == "client-error")

# ---- _cli_json: a zero exit with junk on stdout is a FAILURE, not an empty result --------
import asyncio  # noqa: E402


def _fake_cli(rc: int, out: str, err: str = ""):
    async def run(_args, timeout=20.0):
        return rc, out, err

    return run


main._run_cli = _fake_cli(0, '{"hosts": [{"name": "desk"}]}')
got = asyncio.run(main._cli_json(["discover", "--json"]))
check("json: payload merged under ok", got == {"ok": True, "hosts": [{"name": "desk"}]})

main._run_cli = _fake_cli(0, "not json at all")
got = asyncio.run(main._cli_json(["discover", "--json"]))
check("json: unparseable stdout is an error, not an empty list", got["ok"] is False)
check("json: ...and says so specifically", got["error"] == "client-error")

main._run_cli = _fake_cli(5, "", 'unknown command "discover"')
got = asyncio.run(main._cli_json(["discover", "--json"]))
check("json: old client surfaces as client-outdated", got["error"] == "client-outdated")
check("json: detail carries the CLI's own last line", "unknown command" in got["detail"])

# ---- _field_from (flatpak info parsing, drives the client update check) ------------------
info = "        ID: io.unom.Punktfunk\n    Origin: punktfunk-origin\n    Commit: abc123def\n"
check("field: commit", main._field_from(info, "Commit") == "abc123def")
check("field: origin", main._field_from(info, "Origin") == "punktfunk-origin")
check("field: absent", main._field_from(info, "Nope") == "")

# ---- _looks_outdated (the GTK-init signature of a client predating a headless flag) ------
check("outdated: gtk init noise", main._looks_outdated("cannot open display: \nGtk-WARNING") is True)
check("outdated: an ordinary error is not", main._looks_outdated("connection refused") is False)

# ---- _semver_tuple (plugin update comparison) --------------------------------------------
check("semver: plain", main._semver_tuple("1.2.3") == (1, 2, 3))
check("semver: pre-release suffix dropped", main._semver_tuple("1.2.3-rc1") == (1, 2, 3))
check("semver: short forms pad", main._semver_tuple("2") == (2, 0, 0))
check("semver: ordering", main._semver_tuple("0.10.0") > main._semver_tuple("0.9.9"))

# ---- _upsert_configset_entry (Steam Input layout binding) --------------------------------
#
# Untested until now, and the riskiest thing that survived the cut: it edits a file holding
# HUNDREDS of other games' controller bindings, in place. Every assertion below is about not
# touching them.
empty = main._upsert_configset_entry("", "punktfunk", "template", "punktfunk.vdf")
check("vdf: builds the skeleton when the file is new", '"controller_config"' in empty)
check("vdf: the entry lands", '"punktfunk"' in empty and '"punktfunk.vdf"' in empty)

existing = (
    '"controller_config"\n'
    "{\n"
    '\t"halflife2"\n'
    "\t{\n"
    '\t\t"template"\t\t"other.vdf"\n'
    "\t}\n"
    "}\n"
)
added = main._upsert_configset_entry(existing, "punktfunk", "template", "punktfunk.vdf")
check("vdf: an existing game's entry survives insertion", '"halflife2"' in added)
check("vdf: ours is inserted", '"punktfunk"' in added)

# Re-running must REPLACE our block, not accumulate a second one (this runs on every plugin
# session gated only by a localStorage marker, so idempotence is the whole contract).
twice = main._upsert_configset_entry(added, "punktfunk", "template", "punktfunk.vdf")
check("vdf: idempotent", twice.count('"punktfunk"\n') == 1)
check("vdf: neighbour still intact after the rewrite", '"halflife2"' in twice)

# Steam keys non-Steam games by their LOWERCASE name, and files on disk may carry either case —
# a case-sensitive match would append a duplicate the game never reads.
mixed = existing.replace('"halflife2"', '"Punktfunk"')
replaced = main._upsert_configset_entry(mixed, "punktfunk", "template", "punktfunk.vdf")
check("vdf: matches an existing key case-insensitively", replaced.count("unktfunk\"\n") == 1)

print()
if failures:
    print(f"{failures} check(s) FAILED")
    sys.exit(1)
print("all checks passed")
