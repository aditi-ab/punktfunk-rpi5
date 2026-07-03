#!/usr/bin/env python3
"""Unit checks for main.py's pure helpers — stdlib only, no Decky runtime needed.

Stubs the ``decky`` module (main.py imports it at module level), then asserts the
avahi/TSV/error parsers against fixture strings. The LibraryError fixtures are pinned to
the REAL Display strings in clients/linux/src/library.rs — if those are reworded, the
classifier degrades to ``client-error`` and the matching assertion here fails on purpose.

    python3 clients/decky/scripts/test-backend.py
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


# ---- _parse_library_tsv -----------------------------------------------------------------
tsv = (
    "steam:570\tsteam\tDota 2\n"
    "custom:abc\tcustom\tTabs\tin\ttitle\n"  # tabs inside the title survive (split max 2)
    "2 game(s)\n"  # the count trailer has no tabs — self-skips
)
games = main._parse_library_tsv(tsv)
check("tsv: two games parsed", len(games) == 2)
check("tsv: fields", games[0] == {"id": "steam:570", "store": "steam", "title": "Dota 2"})
check("tsv: tabs in title preserved", games[1]["title"] == "Tabs\tin\ttitle")
check("tsv: empty input", main._parse_library_tsv("0 game(s)\n") == [])

# ---- _classify_library_error (fixtures = library.rs Display strings) --------------------
check(
    "err: not-paired",
    main._classify_library_error(
        "library: The host didn't recognize this device. Pair with the host first — the "
        "library is authorized by this device's certificate (no token needed)."
    )
    == "not-paired",
)
check(
    "err: pin-mismatch",
    main._classify_library_error(
        "library: The host's certificate doesn't match the pinned fingerprint. "
        "Re-pair with a PIN to re-establish trust."
    )
    == "pin-mismatch",
)
check(
    "err: unreachable",
    main._classify_library_error(
        "library: Couldn't reach the host's management API: connection refused. Check the "
        "host is updated and reachable."
    )
    == "unreachable",
)
check(
    "err: http",
    main._classify_library_error("library: The management API returned HTTP 500.") == "http",
)
check(
    "err: outdated client (GTK init noise)",
    main._classify_library_error("cannot open display: \nGtk-WARNING: init failed")
    == "client-outdated",
)
check("err: generic fallback", main._classify_library_error("boom") == "client-error")

# ---- _parse_avahi_browse (incl. the new id/mgmt TXT keys) --------------------------------
avahi = (
    "+;eth0;IPv4;living-room;_punktfunk._udp;local\n"
    "=;eth0;IPv4;living-room;_punktfunk._udp;local;lr.local;192.168.1.42;9777;"
    '"proto=punktfunk/1" "fp=aabbcc" "pair=required" "id=abc123" "mgmt=47990"\n'
    "=;eth0;IPv6;living-room;_punktfunk._udp;local;lr.local;fe80::1;9777;"
    '"proto=punktfunk/1" "fp=aabbcc" "pair=required" "id=abc123" "mgmt=47990"\n'
    "=;eth0;IPv4;bare-host;_punktfunk._udp;local;bh.local;192.168.1.77;9777;"
    '"proto=punktfunk/1" "fp=ddeeff" "pair=optional"\n'
)
hosts = main._parse_avahi_browse(avahi)
check("avahi: two hosts (id-dedup, IPv4 preferred)", len(hosts) == 2)
lr = next(h for h in hosts if h["name"] == "living-room")
check("avahi: ipv4 wins", lr["host"] == "192.168.1.42")
check("avahi: mgmt parsed", lr["mgmt"] == 47990)
check("avahi: id parsed", lr["id"] == "abc123")
bare = next(h for h in hosts if h["name"] == "bare-host")
check("avahi: mgmt absent -> 0", bare["mgmt"] == 0)
check("avahi: id absent -> empty", bare["id"] == "")

# ---- pins store (round-trip through the real methods, isolated HOME) --------------------
import asyncio  # noqa: E402
import shutil  # noqa: E402

shutil.rmtree(decky.DECKY_USER_HOME, ignore_errors=True)
plugin = main.Plugin()
pin = {
    "game_id": "steam:570",
    "title": "Dota 2",
    "store": "steam",
    "host_fp": "AABBCC",
    "host_id": "abc123",
    "host_name": "living-room",
    "host": "192.168.1.42",
    "port": 9777,
    "mgmt": 47990,
    "added_at": 1780000000,
}
dupe = dict(pin, title="Dota 2 again")
junk = {"title": "no game id"}
res = asyncio.run(plugin.set_pins([pin, dupe, junk]))
check("pins: write ok", res.get("ok") is True)
got = asyncio.run(plugin.get_pins())["pins"]
check("pins: dedup + junk dropped", len(got) == 1)
check("pins: unpaired without known-hosts", got[0]["paired"] is False)
# Mark the host paired in the client's known-hosts store — get_pins must pick it up.
cfg = main._client_config_dir()
cfg.mkdir(parents=True, exist_ok=True)
(cfg / "client-known-hosts.json").write_text(
    '{"hosts": [{"name": "living-room", "addr": "192.168.1.42", "port": 9777, '
    '"fp_hex": "aabbcc", "paired": true}]}'
)
got = asyncio.run(plugin.get_pins())["pins"]
check("pins: paired via known-hosts fp (case-insensitive)", got[0]["paired"] is True)
shutil.rmtree(decky.DECKY_USER_HOME, ignore_errors=True)

print()
if failures:
    print(f"{failures} check(s) FAILED")
    sys.exit(1)
print("all checks passed")
