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
* **check_update()** — poll the registry's per-channel ``manifest.json`` and report whether a
  newer build is available (the frontend then drives Decky's own install RPC to apply it).

The TXT-record keys parsed (``proto`` / ``fp`` / ``pair`` / ``id``) are defined by the host
advert in ``crates/punktfunk-host/src/discovery.rs``.
"""

import asyncio
import base64
import json
import os
import shutil
import ssl
import time
import urllib.request
from pathlib import Path

import decky

# Flatpak application id of the GTK client (packaging/flatpak/io.unom.Punktfunk.yml).
APP_ID = "io.unom.Punktfunk"

# Service type advertised by punktfunk/1 hosts (matches NATIVE_SERVICE in the Rust host).
SERVICE_TYPE = "_punktfunk._udp"

# The flatpak client persists identity / known-hosts / settings under HOME/.config/punktfunk.
# The sandbox HOME resolves to the REAL user home (== DECKY_USER_HOME), NOT the per-app
# ~/.var/app/<APP_ID> dir — verified on-device (`flatpak run … sh -c 'echo $HOME'` prints
# /home/deck, and the manifest's `--filesystem=~/.config/punktfunk` grants exactly that path;
# we also pass HOME=DECKY_USER_HOME into `flatpak run`, see _flatpak_env). Pointing here is what
# lets plugin settings actually reach the client AND lets us read the client's known-hosts to
# tell whether THIS device is already paired with a given host.
def _client_config_dir() -> Path:
    return Path(decky.DECKY_USER_HOME) / ".config" / "punktfunk"


def _settings_path() -> Path:
    return _client_config_dir() / "client-gtk-settings.json"


def _paired_fingerprints() -> set[str]:
    """Host cert fingerprints (lowercase hex) this client has PIN-paired, from the client's
    known-hosts store. Keyed by fingerprint so it survives a host changing IP address."""
    try:
        data = json.loads((_client_config_dir() / "client-known-hosts.json").read_text())
    except (OSError, json.JSONDecodeError):
        return set()
    hosts = data.get("hosts", []) if isinstance(data, dict) else []
    return {
        h["fp_hex"].lower()
        for h in hosts
        if isinstance(h, dict) and h.get("paired") and isinstance(h.get("fp_hex"), str)
    }


def _runner_path() -> str:
    """Absolute path to the launch wrapper shipped with the plugin (bin/punktfunkrun.sh)."""
    return str(Path(decky.DECKY_PLUGIN_DIR) / "bin" / "punktfunkrun.sh")


# ----------------------------------------------------------------------------------------
# Self-update check (no Decky store). The plugin is distributed via "Install Plugin from
# URL" pointing at our Gitea generic registry, so the official store never sees it and
# can't offer updates. Instead the backend polls a tiny per-channel ``manifest.json`` the
# CI publishes next to the zip, compares it to the installed version, and the frontend
# offers a one-tap update that drives Decky's own (root, privileged) install RPC. The
# channel + manifest URL are baked into ``update.json`` by CI (.gitea/workflows/decky.yml);
# a dev/sideload build has no ``update.json`` and update checks are simply disabled.
_UPDATE_TTL_S = 1800.0  # cache a successful check for 30 min (the QAM remounts often)
_update_cache: dict = {"at": 0.0, "data": None}


def _update_config() -> dict:
    """The CI-baked ``{channel, manifest}`` next to the plugin (absent on dev builds)."""
    try:
        return json.loads((Path(decky.DECKY_PLUGIN_DIR) / "update.json").read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def _installed_version() -> str:
    """The version Decky itself reports for this plugin — it reads ``package.json`` (NOT
    plugin.json), so the CI stamps the build version there."""
    try:
        pkg = json.loads((Path(decky.DECKY_PLUGIN_DIR) / "package.json").read_text())
        return str(pkg.get("version", "0.0.0"))
    except (OSError, json.JSONDecodeError):
        return "0.0.0"


def _semver_tuple(v: str) -> tuple[int, int, int]:
    """A tolerant (major, minor, patch) tuple for ``>`` comparison. We control the version
    format (plain numeric ``X.Y.Z`` on both channels), so leading-int-per-component is
    enough; any pre-release suffix is dropped before comparing."""
    parts: list[int] = []
    for comp in str(v).split("-", 1)[0].split(".")[:3]:
        digits = ""
        for ch in comp:
            if ch.isdigit():
                digits += ch
            else:
                break
        parts.append(int(digits) if digits else 0)
    while len(parts) < 3:
        parts.append(0)
    return (parts[0], parts[1], parts[2])


# Decky Loader ships its own embedded (PyInstaller) Python whose compiled-in OpenSSL default
# verify paths don't exist on SteamOS — ``ssl.create_default_context()`` then trusts NOTHING
# and every HTTPS fetch dies with CERTIFICATE_VERIFY_FAILED (seen live on the Deck). Fix: find
# a real CA bundle on disk and load it explicitly. Verification is NEVER disabled — if no
# bundle exists the fetch just fails, and check_update() is non-fatal by design.
_CA_BUNDLES = (
    "/etc/ssl/certs/ca-certificates.crt",  # SteamOS / Arch / Debian / Ubuntu
    "/etc/ssl/cert.pem",  # Arch/openssl compat symlink
    "/etc/pki/tls/certs/ca-bundle.crt",  # Fedora / Bazzite
    "/etc/ssl/ca-bundle.pem",  # openSUSE
)
_ssl_context_cache: ssl.SSLContext | None = None


def _build_ssl_context() -> ssl.SSLContext:
    """A verifying SSLContext that actually has CA roots under Decky's embedded Python."""
    ctx = ssl.create_default_context()  # honors SSL_CERT_FILE / SSL_CERT_DIR when set
    if ctx.cert_store_stats().get("x509_ca", 0):
        return ctx  # the interpreter found its own roots (e.g. a system python)

    dvp = ssl.get_default_verify_paths()
    candidates: list[str | None] = [dvp.cafile, dvp.openssl_cafile, *_CA_BUNDLES]
    try:  # not shipped by Decky's runtime, but honor it when importable
        import certifi

        candidates.append(certifi.where())
    except ImportError:
        pass

    tried: set[str] = set()
    for cafile in candidates:
        if not cafile or cafile in tried or not Path(cafile).is_file():
            continue
        tried.add(cafile)
        try:
            ctx.load_verify_locations(cafile=cafile)
        except (ssl.SSLError, OSError):
            continue
        if ctx.cert_store_stats().get("x509_ca", 0):
            decky.logger.info("TLS roots loaded from %s", cafile)
            return ctx

    decky.logger.warning(
        "no CA bundle found — HTTPS update checks will fail certificate verification"
    )
    return ctx


def _ssl_context() -> ssl.SSLContext:
    """The (cached) context for registry fetches; building it scans disk, so do it once."""
    global _ssl_context_cache
    if _ssl_context_cache is None:
        _ssl_context_cache = _build_ssl_context()
    return _ssl_context_cache


def _fetch_json(url: str, timeout: float = 8.0) -> dict:
    """Blocking HTTPS GET of a small JSON document (run in an executor)."""
    req = urllib.request.Request(
        url, headers={"Accept": "application/json", "User-Agent": "punktfunk-decky"}
    )
    with urllib.request.urlopen(req, timeout=timeout, context=_ssl_context()) as resp:
        return json.loads(resp.read().decode("utf-8", errors="replace"))


def _flatpak() -> str | None:
    return shutil.which("flatpak") or (
        "/usr/bin/flatpak" if Path("/usr/bin/flatpak").exists() else None
    )


def _flatpak_env() -> dict:
    """Environment for a headless ``flatpak run`` from the backend (no display needed for
    pairing). Reconstruct the user-session bits flatpak wants; the backend may not inherit
    them. Harmless if some are already set."""
    env = dict(os.environ)
    # Decky Loader is a PyInstaller binary: it prepends its bundled libs (an older libssl) to
    # LD_LIBRARY_PATH (its /tmp/_MEI* unpack dir), and that env leaks into our subprocess. The
    # SYSTEM flatpak's libcurl needs OPENSSL_3.3.0 from the SYSTEM libssl, so the bundled libssl
    # breaks it ("libssl.so.3: version OPENSSL_3.3.0 not found"). Restore the pre-bundle value
    # PyInstaller saved as <VAR>_ORIG, or drop the var so the dynamic loader uses system libraries.
    for var in ("LD_LIBRARY_PATH", "LD_PRELOAD"):
        orig = env.pop(f"{var}_ORIG", None)
        if orig:
            env[var] = orig
        else:
            env.pop(var, None)
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


async def _flatpak_capture(args: list[str], timeout: float = 20.0) -> tuple[int, str]:
    """Run ``flatpak <args>`` with the user-session env, merging stderr into stdout. Returns
    ``(returncode, output)``; ``(-1, "")`` if the binary is missing or the call errors/times out.
    Best-effort by design — every caller here treats a failure as "no update / can't tell"."""
    flatpak = _flatpak()
    if not flatpak:
        return -1, ""
    proc = None
    try:
        proc = await asyncio.create_subprocess_exec(
            flatpak, *args,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
            env=_flatpak_env(),
        )
        out, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        rc = proc.returncode if proc.returncode is not None else -1
        return rc, (out or b"").decode("utf-8", "replace")
    except asyncio.TimeoutError:
        decky.logger.warning("flatpak %s timed out", " ".join(args))
        if proc:
            try:
                proc.kill()
            except ProcessLookupError:
                pass
        return -1, ""
    except Exception:  # noqa: BLE001
        decky.logger.exception("flatpak %s failed", " ".join(args))
        return -1, ""


def _field_from(text: str, name: str) -> str:
    """Pull ``<name>: value`` out of ``flatpak info`` / ``remote-info`` output (e.g. ``Commit``,
    ``Origin``)."""
    prefix = f"{name}:"
    for line in text.splitlines():
        s = line.strip()
        if s.startswith(prefix):
            return s.split(":", 1)[1].strip()
    return ""


async def _client_update_state() -> dict:
    """Is a newer commit of the flatpak client available in the remote it tracks? The client is a
    **per-user** install (so ``sudo flatpak update``, which is system-scope, never touches it), and
    it versions independently of this plugin — so we compare the installed commit against the
    remote's here and let the QAM offer a user-scope update. Best-effort; all-``False`` on any error
    (not installed, no flatpak, offline)."""
    state = {"available": False, "installed": "", "remote": ""}
    rc, info = await _flatpak_capture(["info", "--user", APP_ID], timeout=10.0)
    if rc != 0:
        return state  # client not installed as a user app / no flatpak
    state["installed"] = _field_from(info, "Commit")
    origin = _field_from(info, "Origin")
    if not origin:
        return state
    rc, rinfo = await _flatpak_capture(["remote-info", "--user", origin, APP_ID], timeout=25.0)
    if rc != 0:
        return state  # remote unreachable — treat as "up to date", retry next check
    state["remote"] = _field_from(rinfo, "Commit")
    state["available"] = bool(
        state["installed"] and state["remote"] and state["installed"] != state["remote"]
    )
    return state


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
        # Mark which hosts THIS device has already paired (by cert fingerprint), so the UI can
        # show "Stream" instead of "Pair" — the mDNS `pair` field is the host's policy, not our
        # per-device pairing state.
        paired = _paired_fingerprints()
        for h in hosts:
            fp = h.get("fp") or ""
            h["paired"] = bool(fp) and fp.lower() in paired
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

    async def shortcut_art(self) -> dict:
        """The Steam-shortcut artwork shipped with the plugin (``assets/``, generated by
        ``scripts/gen-steam-art.py``): base64 PNGs for SetCustomArtworkForApp plus the
        icon's absolute path for SetShortcutIcon (which wants a file, not bytes). Missing
        files are simply omitted — artwork is cosmetic and must never block a launch."""
        art: dict = {}
        base = Path(decky.DECKY_PLUGIN_DIR) / "assets"
        for key, fname in (
            ("grid", "grid.png"),
            ("gridwide", "gridwide.png"),
            ("hero", "hero.png"),
            ("logo", "logo.png"),
        ):
            try:
                art[key] = base64.b64encode((base / fname).read_bytes()).decode()
            except OSError:
                pass
        icon = base / "icon.png"
        art["icon_path"] = str(icon) if icon.exists() else ""
        return art

    async def runner_info(self) -> dict:
        """The wrapper-script path + flatpak app id the frontend needs to create the Steam
        shortcut. The shortcut invokes the script through ``/bin/sh`` (see steam.ts), so no
        exec bit is needed — Decky's zip extraction drops it, and the root-owned plugins dir
        means this unprivileged backend couldn't chmod it back on anyway."""
        path = _runner_path()
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

    async def update_client(self) -> dict:
        """Update the flatpak **client** (io.unom.Punktfunk) in the USER installation — the scope a
        Steam Deck install lives in, which ``sudo flatpak update`` (system-scope) never reaches.
        Returns whether a new commit was actually pulled. Best-effort; non-fatal."""
        flatpak = _flatpak()
        if not flatpak:
            return {"ok": False, "updated": False, "error": "flatpak-not-found"}
        _, before = await _flatpak_capture(["info", "--user", APP_ID], timeout=10.0)
        before_commit = _field_from(before, "Commit")
        rc, out = await _flatpak_capture(["update", "--user", "-y", APP_ID], timeout=300.0)
        if rc != 0:
            decky.logger.warning("flatpak client update failed (rc=%s): %s", rc, out[-400:])
            return {"ok": False, "updated": False, "error": "update-failed"}
        _, after = await _flatpak_capture(["info", "--user", APP_ID], timeout=10.0)
        after_commit = _field_from(after, "Commit")
        updated = bool(before_commit and after_commit and before_commit != after_commit)
        decky.logger.info(
            "flatpak client update: %s -> %s (updated=%s)",
            before_commit[:10], after_commit[:10], updated,
        )
        _update_cache["data"] = None  # invalidate the cached "update available" snapshot
        return {"ok": True, "updated": updated}

    async def check_update(self, force: bool = False) -> dict:
        """Report pending updates for BOTH the plugin and the flatpak client.

        The plugin updates via Decky's install RPC (the per-channel ``manifest.json`` the CI
        publishes); the **client** updates via ``flatpak update --user`` (a per-user install, so
        ``sudo flatpak update`` — system-scope — never touches it) and versions independently, so
        it's checked here too and applied through :meth:`update_client`. Non-fatal: any failure
        leaves the respective ``*_update_available`` ``False``.
        """
        current = _installed_version()
        cfg = _update_config()
        result = {
            "current": current,
            "latest": current,
            "artifact": "",
            "hash": "",
            "channel": str(cfg.get("channel", "")),
            "update_available": False,
            "client_update_available": False,
            "client_current": "",
            "client_latest": "",
        }

        now = time.monotonic()
        cached = _update_cache["data"]
        if not force and cached and (now - _update_cache["at"]) < _UPDATE_TTL_S:
            return cached

        # Client (flatpak) update — checked ALWAYS, even on a dev/sideloaded plugin build.
        try:
            cu = await _client_update_state()
            result["client_update_available"] = bool(cu["available"])
            result["client_current"] = (cu["installed"] or "")[:10]
            result["client_latest"] = (cu["remote"] or "")[:10]
        except Exception:  # noqa: BLE001
            decky.logger.warning("client update check failed", exc_info=True)

        manifest_url = cfg.get("manifest")
        if not manifest_url:
            result["error"] = "update-channel-unknown"  # dev / sideloaded plugin build
            _update_cache["at"] = now
            _update_cache["data"] = result  # the client info is still valid to cache
            return result

        try:
            loop = asyncio.get_running_loop()
            manifest = await loop.run_in_executor(None, _fetch_json, manifest_url)
        except Exception as exc:  # noqa: BLE001
            decky.logger.warning("plugin update check failed: %s", exc)
            result["error"] = "fetch-failed"
            return result  # transient — don't cache, retry next open

        latest = str(manifest.get("version", current))
        result["latest"] = latest
        result["artifact"] = str(manifest.get("artifact", ""))
        result["hash"] = str(manifest.get("sha256", ""))
        result["update_available"] = bool(result["artifact"]) and (
            _semver_tuple(latest) > _semver_tuple(current)
        )
        if result["update_available"] or result["client_update_available"]:
            decky.logger.info(
                "updates: plugin %s->%s (avail=%s), client->%s (avail=%s)",
                current, latest, result["update_available"],
                result["client_latest"], result["client_update_available"],
            )
        _update_cache["at"] = now
        _update_cache["data"] = result
        return result

    # ---- Decky lifecycle ----

    async def _main(self):
        decky.logger.info("punktfunk plugin loaded (runner=%s)", _runner_path())

    async def _unload(self):
        decky.logger.info("punktfunk plugin unloading")

    async def _uninstall(self):
        decky.logger.info("punktfunk plugin uninstalled")
