"""
punktfunk Decky plugin — backend.

The Gaming-Mode UI (``src/index.tsx``) calls these methods over the Decky bridge. The actual
STREAM is NOT launched here — it is launched by the frontend through Steam
(SteamClient.Apps.RunGame on a hidden non-Steam shortcut that points at ``bin/punktfunkrun.sh``),
because gamescope only focuses/fullscreens windows in the process tree Steam launched via
``reaper``. A flatpak spawned from this backend would be invisible/unfocused (gamescope#484).
The backend's jobs are the things Steam can't do:

* **discover()** — browse the LAN over mDNS (``avahi-browse``) for ``_punktfunk._udp`` hosts.
* **pair(host, port, pin, name)** — run the SPAKE2 PIN ceremony headlessly via the flatpak
  client's ``--pair`` mode, capturing the result. Pairing uses the SAME flatpak (so the same
  identity store the stream uses), so once paired the stream connects silently.
* **runner_info()** — the absolute path to the launch wrapper + the flatpak app id, handed to
  the frontend so it can create/point the Steam shortcut.
* **get_settings() / set_settings()** — read/write the flatpak client's stream settings JSON
  (resolution / bitrate / gamepad), so the Deck UI configures the stream the client reads.
* **kill_stream()** — force-stop a wedged stream (``flatpak kill``).

The TXT-record keys parsed (``proto`` / ``fp`` / ``pair`` / ``id``) are defined by the host
advert in ``crates/punktfunk-host/src/discovery.rs``.
"""

import asyncio
import json
import os
import shutil
import stat
from pathlib import Path

import decky

# Flatpak application id of the GTK client (packaging/flatpak/io.unom.Punktfunk.yml).
APP_ID = "io.unom.Punktfunk"

# Service type advertised by punktfunk/1 hosts (matches NATIVE_SERVICE in the Rust host).
SERVICE_TYPE = "_punktfunk._udp"

# The flatpak client persists identity / known-hosts / settings under HOME/.config/punktfunk;
# inside the flatpak sandbox HOME is ~/.var/app/<APP_ID>, so the real on-disk location is this.
# The backend writes settings here so the (sandboxed) client reads them.
def _client_config_dir() -> Path:
    return Path(decky.DECKY_USER_HOME) / ".var" / "app" / APP_ID / ".config" / "punktfunk"


def _settings_path() -> Path:
    return _client_config_dir() / "client-gtk-settings.json"


def _runner_path() -> str:
    """Absolute path to the launch wrapper shipped with the plugin (bin/punktfunkrun.sh)."""
    return str(Path(decky.DECKY_PLUGIN_DIR) / "bin" / "punktfunkrun.sh")


def _flatpak() -> str | None:
    return shutil.which("flatpak") or (
        "/usr/bin/flatpak" if Path("/usr/bin/flatpak").exists() else None
    )


def _flatpak_env() -> dict:
    """Environment for a headless ``flatpak run`` from the backend (no display needed for
    pairing). Reconstruct the user-session bits flatpak wants; the backend may not inherit
    them. Harmless if some are already set."""
    env = dict(os.environ)
    env.setdefault("HOME", decky.DECKY_USER_HOME)
    uid = os.environ.get("PF_UID") or "1000"
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{uid}")
    env.setdefault(
        "DBUS_SESSION_BUS_ADDRESS", f"unix:path=/run/user/{uid}/bus"
    )
    # Ensure flatpak can find the user installation.
    env.setdefault(
        "PATH", "/usr/bin:/bin:" + env.get("PATH", "")
    )
    return env


def _split_txt(txt: str) -> list[str]:
    """Split an avahi TXT column into tokens, honouring the ``"key=value"`` quoting."""
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


def _parse_avahi_browse(stdout: str) -> list[dict]:
    """Parse ``avahi-browse -rpt`` output into a list of host dicts (deduped on the TXT ``id``)."""
    out: dict[str, dict] = {}
    for raw in stdout.splitlines():
        line = raw.strip()
        if not line.startswith("="):
            continue
        parts = line.replace("\\;", "\x00").split(";")
        parts = [p.replace("\x00", ";") for p in parts]
        if len(parts) < 9:
            continue

        name = parts[3]
        address = parts[7]
        port_str = parts[8]
        txt = parts[9] if len(parts) > 9 else ""

        try:
            port = int(port_str)
        except ValueError:
            port = 0

        props: dict[str, str] = {}
        for token in _split_txt(txt):
            if "=" in token:
                k, v = token.split("=", 1)
                props[k] = v

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
        existing = out.get(key)
        # Prefer IPv4 over IPv6 for the user-facing host string.
        if existing is None or (":" in existing["host"] and ":" not in address):
            out[key] = entry

    return list(out.values())


class Plugin:
    async def discover(self) -> list[dict]:
        """Browse the LAN for punktfunk/1 hosts. Returns ``[{name, host, port, pair, fp}]``."""
        avahi = shutil.which("avahi-browse")
        if not avahi:
            decky.logger.error("avahi-browse not found; install avahi for host discovery")
            return []
        try:
            proc = await asyncio.create_subprocess_exec(
                avahi, "-rpt", SERVICE_TYPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            try:
                stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=8.0)
            except asyncio.TimeoutError:
                proc.kill()
                decky.logger.warning("avahi-browse timed out")
                return []
        except Exception:  # noqa: BLE001
            decky.logger.exception("avahi-browse failed")
            return []
        if stderr:
            decky.logger.debug("avahi-browse stderr: %s", stderr.decode(errors="replace"))
        hosts = _parse_avahi_browse(stdout.decode(errors="replace"))
        decky.logger.info("discovered %d punktfunk host(s)", len(hosts))
        return hosts

    async def pair(self, host: str, port: int, pin: str, name: str = "Steam Deck") -> dict:
        """Run the SPAKE2 PIN ceremony headlessly via the flatpak client's ``--pair`` mode.

        The user arms pairing on the HOST (which displays a 4-digit PIN) and enters it here.
        On success the flatpak persists the host to its known-hosts as paired, so a later
        stream connects silently. Returns ``{ok, fp?, error?}``.
        """
        flatpak = _flatpak()
        if not flatpak:
            return {"ok": False, "error": "flatpak-not-found"}
        argv = [
            flatpak, "run", "--arch=x86_64", APP_ID,
            "--pair", str(pin).strip(),
            "--connect", f"{host}:{port}",
            "--name", name,
            "--host-label", host,
        ]
        decky.logger.info("pairing: %s", " ".join(argv[:6] + ["<pin>", "--connect", f"{host}:{port}"]))
        try:
            proc = await asyncio.create_subprocess_exec(
                *argv,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                env=_flatpak_env(),
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=100.0)
        except asyncio.TimeoutError:
            return {"ok": False, "error": "pairing timed out"}
        except Exception as exc:  # noqa: BLE001
            decky.logger.exception("pairing failed to launch")
            return {"ok": False, "error": str(exc)}

        out = stdout.decode(errors="replace")
        err = stderr.decode(errors="replace")
        if proc.returncode == 0 and "paired " in out:
            fp = ""
            for tok in out.split():
                if tok.startswith("fp="):
                    fp = tok[3:]
            decky.logger.info("paired %s:%s", host, port)
            return {"ok": True, "fp": fp}
        decky.logger.warning("pairing failed (rc=%s): %s", proc.returncode, err.strip() or out.strip())
        # Surface the client's own one-line reason (wrong PIN / not armed) to the UI.
        reason = (err.strip().splitlines() or out.strip().splitlines() or ["pairing failed"])[-1]
        return {"ok": False, "error": reason}

    async def runner_info(self) -> dict:
        """The wrapper-script path + flatpak app id the frontend needs to create the Steam
        shortcut. Also (re)asserts the script's exec bit — packaging can drop it."""
        path = _runner_path()
        try:
            st = os.stat(path)
            os.chmod(path, st.st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        except OSError:
            decky.logger.warning("could not chmod runner %s", path)
        return {"runner": path, "app_id": APP_ID, "exists": Path(path).exists()}

    async def get_settings(self) -> dict:
        """Read the flatpak client's stream settings (resolution/bitrate/gamepad…)."""
        try:
            return json.loads(_settings_path().read_text())
        except (OSError, json.JSONDecodeError):
            # The client's own defaults (native display, host-default bitrate, auto pad).
            return {
                "width": 0, "height": 0, "refresh_hz": 0, "bitrate_kbps": 0,
                "gamepad": "auto", "compositor": "auto",
                "inhibit_shortcuts": True, "mic_enabled": False,
            }

    async def set_settings(self, settings: dict) -> dict:
        """Write the stream settings JSON the (sandboxed) client reads on launch."""
        try:
            d = _client_config_dir()
            d.mkdir(parents=True, exist_ok=True)
            _settings_path().write_text(json.dumps(settings, indent=2))
            return {"ok": True}
        except OSError as exc:
            decky.logger.exception("could not write settings")
            return {"ok": False, "error": str(exc)}

    async def kill_stream(self) -> dict:
        """Force-stop a wedged stream client (``flatpak kill``)."""
        flatpak = _flatpak()
        if not flatpak:
            return {"ok": False, "error": "flatpak-not-found"}
        try:
            proc = await asyncio.create_subprocess_exec(
                flatpak, "kill", APP_ID,
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                env=_flatpak_env(),
            )
            await asyncio.wait_for(proc.wait(), timeout=10.0)
        except Exception:  # noqa: BLE001
            decky.logger.exception("flatpak kill failed")
            return {"ok": False}
        return {"ok": True}

    # ---- Decky lifecycle ----

    async def _main(self):
        decky.logger.info("punktfunk plugin loaded (runner=%s)", _runner_path())

    async def _unload(self):
        decky.logger.info("punktfunk plugin unloading")

    async def _uninstall(self):
        decky.logger.info("punktfunk plugin uninstalled")
