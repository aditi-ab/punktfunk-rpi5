"""
punktfunk Decky plugin — backend.

Bridges the Gaming-Mode Quick Access panel (``src/index.tsx``) to two host-side
operations:

* **discover()** — browse the LAN over mDNS for punktfunk/1 hosts advertising the
  ``_punktfunk._udp`` service, returning name / ip:port / pairing-requirement / cert
  fingerprint for each. Implemented by shelling out to ``avahi-browse`` (SteamOS, Bazzite
  and most Linux distros ship ``avahi-daemon``); see :func:`Plugin.discover`.
* **connect(host, port)** / **disconnect()** — launch / kill the native GTK4 client
  (``punktfunk-client --connect host:port``). The child PID is tracked so a later
  :func:`Plugin.disconnect` (or plugin unload) can terminate it.

The TXT-record keys parsed here (``proto`` / ``fp`` / ``pair`` / ``id``) are defined by the
host advert in ``crates/punktfunk-host/src/discovery.rs``.
"""

import asyncio
import shutil
from pathlib import Path

import decky

# The native punktfunk/1 client binary (the GTK4/libadwaita Linux client, crate
# ``punktfunk-client-linux``). It is resolved at runtime from PATH and a handful of common
# install locations (see :func:`_resolve_client`). If none exist we fall back to this bare
# name and let the spawn fail loudly — install the client on the Deck (.deb / RPM / flatpak)
# or symlink it into ~/.local/bin.
#
# TODO: once a Steam Deck / SteamOS install path for punktfunk-client is settled (likely a
# flatpak, since SteamOS is image-based and /usr is read-only), pin the canonical path here.
CLIENT_BINARY = "punktfunk-client"

# Service type advertised by punktfunk/1 hosts (matches NATIVE_SERVICE in the Rust host).
SERVICE_TYPE = "_punktfunk._udp"

# Candidate locations probed (in order) when the binary is not on PATH. ``$HOME`` is the
# effective user's home as provided by decky.
_CLIENT_CANDIDATES = [
    "/usr/bin/punktfunk-client",
    "/usr/local/bin/punktfunk-client",
    str(Path(decky.HOME) / ".local" / "bin" / "punktfunk-client"),
    # Flatpak: launched via `flatpak run` rather than a path — handled in _resolve_client.
]


def _resolve_client() -> list[str]:
    """Return the argv prefix used to launch the native client.

    Resolution order: PATH → well-known absolute paths → flatpak (if the app id is
    installed) → bare binary name (so the eventual spawn fails with a clear error).
    """
    on_path = shutil.which(CLIENT_BINARY)
    if on_path:
        return [on_path]

    for candidate in _CLIENT_CANDIDATES:
        if Path(candidate).exists():
            return [candidate]

    # Flatpak fallback. The app id is a guess until a flatpak is actually published;
    # `flatpak run <id>` is a no-op-ish failure if it is not installed, which surfaces as a
    # spawn error the user can act on.
    flatpak = shutil.which("flatpak")
    if flatpak:
        return [flatpak, "run", "earth.buehler.punktfunk.Client"]

    decky.logger.warning(
        "punktfunk-client not found on PATH or in %s; falling back to bare name",
        _CLIENT_CANDIDATES,
    )
    return [CLIENT_BINARY]


def _parse_avahi_browse(stdout: str) -> list[dict]:
    """Parse ``avahi-browse -rpt`` output into a list of host dicts.

    ``avahi-browse -r`` resolves services; ``-p`` makes the output parseable (one record
    per line, semicolon-separated, fields escaped with ``\\``); ``-t`` terminates after the
    initial cache dump instead of running forever.

    Resolved records start with ``=`` and have the columns::

        =;iface;protocol;name;type;domain;hostname;address;port;txt

    where ``txt`` is a space-separated list of ``"key=value"`` tokens, each already wrapped
    in double quotes by avahi, e.g. ``"proto=punktfunk/1" "fp=ab12..." "pair=required"``.

    We dedup on the host advert ``id`` TXT key (a host re-advertises across interfaces /
    IPv4+IPv6, producing several ``=`` lines for one logical host); when ``id`` is absent we
    fall back to ``host:port``.
    """
    out: dict[str, dict] = {}
    for raw in stdout.splitlines():
        line = raw.strip()
        if not line.startswith("="):
            continue
        # Split on unescaped ';'. avahi escapes literal ';' inside fields as '\;', so a
        # simple replace-guard split is adequate for the fixed 10-column layout.
        parts = line.replace("\\;", "\x00").split(";")
        parts = [p.replace("\x00", ";") for p in parts]
        if len(parts) < 9:
            continue

        name = parts[3]
        # parts[4] is the service type, parts[5] the domain.
        address = parts[7]
        port_str = parts[8]
        txt = parts[9] if len(parts) > 9 else ""

        try:
            port = int(port_str)
        except ValueError:
            port = 0

        # Parse TXT tokens: each is a quoted "key=value".
        props: dict[str, str] = {}
        for token in _split_txt(txt):
            if "=" in token:
                k, v = token.split("=", 1)
                props[k] = v

        # Only surface actual punktfunk/1 adverts.
        if props.get("proto") and not props["proto"].startswith("punktfunk/"):
            continue

        entry = {
            "name": name,
            "host": address,
            "port": port,
            "pair": props.get("pair", "optional"),
            "fp": props.get("fp", ""),
            "proto": props.get("proto", ""),
        }
        key = props.get("id") or f"{address}:{port}"
        # Prefer an IPv4 record over IPv6 for the user-facing host string when both exist.
        existing = out.get(key)
        if existing is None or (":" in existing["host"] and ":" not in address):
            out[key] = entry

    return list(out.values())


def _split_txt(txt: str) -> list[str]:
    """Split an avahi TXT column into tokens, honouring the ``"key=value"`` quoting.

    avahi prints each TXT item wrapped in double quotes and space-separated, e.g.::

        "proto=punktfunk/1" "fp=ab12cd" "pair=required" "id=host-1"

    A value can legitimately contain spaces, so we split on the quote boundaries rather
    than on whitespace.
    """
    tokens: list[str] = []
    cur: list[str] = []
    in_quote = False
    for ch in txt:
        if ch == '"':
            if in_quote:
                tokens.append("".join(cur))
                cur = []
            in_quote = not in_quote
        elif in_quote:
            cur.append(ch)
    if cur:
        tokens.append("".join(cur))
    return tokens


class Plugin:
    # Tracks the launched native client so disconnect()/_unload can terminate it.
    _client: asyncio.subprocess.Process | None = None
    _connected_host: str | None = None

    async def discover(self) -> list[dict]:
        """Browse the LAN for punktfunk/1 hosts. Returns ``[{name, host, port, pair, fp}]``."""
        avahi = shutil.which("avahi-browse")
        if not avahi:
            decky.logger.error("avahi-browse not found; install avahi for host discovery")
            return []

        try:
            proc = await asyncio.create_subprocess_exec(
                avahi,
                "-rpt",
                SERVICE_TYPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            try:
                stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=8.0)
            except asyncio.TimeoutError:
                proc.kill()
                decky.logger.warning("avahi-browse timed out")
                return []
        except Exception:  # noqa: BLE001 - surface any spawn failure as "no hosts"
            decky.logger.exception("avahi-browse failed")
            return []

        if stderr:
            decky.logger.debug("avahi-browse stderr: %s", stderr.decode(errors="replace"))

        hosts = _parse_avahi_browse(stdout.decode(errors="replace"))
        decky.logger.info("discovered %d punktfunk host(s)", len(hosts))
        return hosts

    async def connect(self, host: str, port: int) -> dict:
        """Launch the native client against ``host:port``. Returns ``{ok, host, error?}``."""
        # Tear down any prior session first.
        await self.disconnect()

        argv = _resolve_client() + ["--connect", f"{host}:{port}"]
        decky.logger.info("launching client: %s", " ".join(argv))
        try:
            self._client = await asyncio.create_subprocess_exec(
                *argv,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except FileNotFoundError:
            decky.logger.error("client binary not found: %s", argv[0])
            return {"ok": False, "host": f"{host}:{port}", "error": "client-not-found"}
        except Exception as exc:  # noqa: BLE001
            decky.logger.exception("failed to launch client")
            return {"ok": False, "host": f"{host}:{port}", "error": str(exc)}

        self._connected_host = f"{host}:{port}"
        decky.logger.info("client launched (pid %s) -> %s", self._client.pid, self._connected_host)
        return {"ok": True, "host": self._connected_host}

    async def disconnect(self) -> dict:
        """Terminate the launched native client, if any."""
        proc = self._client
        self._client = None
        host = self._connected_host
        self._connected_host = None
        if proc is None or proc.returncode is not None:
            return {"ok": True, "host": None}

        decky.logger.info("disconnecting client (pid %s)", proc.pid)
        try:
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                decky.logger.warning("client did not exit; killing (pid %s)", proc.pid)
                proc.kill()
                await proc.wait()
        except ProcessLookupError:
            pass
        except Exception:  # noqa: BLE001
            decky.logger.exception("error terminating client")
        return {"ok": True, "host": host}

    async def status(self) -> dict:
        """Return the current connection status for UI refresh on panel open."""
        connected = self._client is not None and self._client.returncode is None
        return {"connected": connected, "host": self._connected_host if connected else None}

    # ---- Decky lifecycle ----

    async def _main(self):
        decky.logger.info("punktfunk plugin loaded")

    async def _unload(self):
        decky.logger.info("punktfunk plugin unloading; tearing down client")
        await self.disconnect()

    async def _uninstall(self):
        decky.logger.info("punktfunk plugin uninstalled")
