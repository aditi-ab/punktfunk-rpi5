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
* **library(host, mgmt_port, fp)** — fetch a paired host's game library headlessly via the
  flatpak client's ``--library`` mode (mTLS with the client's own identity; TSV on stdout),
  so the picker UI can offer games to pin.
* **get_pins() / set_pins()** — the pinned-games store (``decky-pinned.json`` next to the
  client's config, so pins survive plugin reinstalls), annotated with live pairing state.
* **runner_info()** — the absolute path to the launch wrapper + the flatpak app id, handed to
  the frontend so it can create/point the Steam shortcut.
* **get_settings() / set_settings()** — read/write the flatpak client's stream settings JSON
  (resolution / bitrate / gamepad), so the Deck UI configures the stream the client reads.
* **kill_stream()** — force-stop a wedged stream (``flatpak kill``).
* **check_update()** — poll the registry's per-channel ``manifest.json`` and report whether a
  newer build is available (the frontend then drives Decky's own install RPC to apply it).

The TXT-record keys parsed (``proto`` / ``fp`` / ``pair`` / ``id`` / ``mgmt``) are defined by
the host advert in ``crates/punktfunk-host/src/discovery.rs``.
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


def _pins_path() -> Path:
    """The pinned-games store — plugin-owned, but deliberately in the CLIENT's config dir
    (like everything else we persist): the plugins dir is root-owned and wiped on
    reinstall, while ``~/.config/punktfunk`` survives both."""
    return _client_config_dir() / "decky-pinned.json"


def _parse_library_tsv(stdout: str) -> list[dict]:
    """Parse the flatpak client's ``--library`` output: one ``id\\tstore\\ttitle`` line per
    game plus a trailing ``N game(s)`` count line (no tabs — it self-skips here). A title
    may itself contain tabs, so split at most twice."""
    games: list[dict] = []
    for line in stdout.splitlines():
        parts = line.split("\t", 2)
        if len(parts) == 3:
            games.append({"id": parts[0], "store": parts[1], "title": parts[2]})
    return games


def _classify_library_error(stderr: str) -> str:
    """Map the client's ``library: <LibraryError Display>`` stderr line to a stable error
    code for the UI. Substring-matched against the Display strings in
    ``crates/pf-client-core/src/library.rs`` — a wording change degrades to ``client-error``
    (generic copy), never a crash."""
    s = stderr.lower()
    if "didn't recognize this device" in s:
        return "not-paired"
    if "pinned fingerprint" in s:
        return "pin-mismatch"
    if "couldn't reach the host" in s:
        return "unreachable"
    if "management api returned http" in s:
        return "http"
    if "display" in s or "gtk" in s:
        # A flatpak so old it predates --library falls through to GTK init, which fails
        # headless from this backend.
        return "client-outdated"
    return "client-error"


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


async def _run_client(client_args: list[str], timeout: float = 20.0) -> tuple[int, str, str]:
    """Run the flatpak CLIENT headlessly (``flatpak run … io.unom.Punktfunk <client_args>``) with
    the user-session env, returning ``(returncode, stdout, stderr)`` with SEPARATE pipes so a JSON
    payload on stdout stays clean of the client's log lines on stderr. ``(-1, "", "")`` when
    flatpak is missing or the call errors/times out. This is the single entry point for the
    headless host-store modes (``--list-hosts`` / ``--add-host`` / ``--set-host`` /
    ``--forget-host`` / ``--reset`` / ``--reachable``), which mutate the SAME
    ``client-known-hosts.json`` the desktop client reads — so state is shared, not duplicated."""
    flatpak = _flatpak()
    if not flatpak:
        return -1, "", ""
    argv = [flatpak, "run", "--arch=x86_64", APP_ID, *client_args]
    proc = None
    try:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            env=_flatpak_env(),
        )
        out, err = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        rc = proc.returncode if proc.returncode is not None else -1
        return (
            rc,
            (out or b"").decode("utf-8", "replace"),
            (err or b"").decode("utf-8", "replace"),
        )
    except asyncio.TimeoutError:
        decky.logger.warning("client %s timed out", " ".join(client_args))
        if proc:
            try:
                proc.kill()
            except ProcessLookupError:
                pass
        return -1, "", ""
    except Exception:  # noqa: BLE001
        decky.logger.exception("client %s failed", " ".join(client_args))
        return -1, "", ""


# The QAM panel and the full page each mount their own hosts view, and Gaming Mode remounts the
# QAM often — every mount calls list_hosts, which spawns a flatpak cold-start plus a reachability
# probe. Cache the last result briefly so back-to-back opens reuse it instead of re-probing; any
# mutation (add/edit/forget/reset/pair) invalidates it so a change shows up immediately.
_HOSTS_TTL_S = 12.0
_hosts_cache: dict = {"at": 0.0, "probed": None, "data": None}


def _invalidate_hosts_cache() -> None:
    _hosts_cache["data"] = None


def _read_known_hosts() -> list[dict]:
    """The saved-hosts store read straight off disk — the fallback for a client too old to have
    ``--list-hosts``. Same file the desktop client owns; `online` is left ``None`` (unknown)
    because a direct read has no reachability signal."""
    try:
        data = json.loads((_client_config_dir() / "client-known-hosts.json").read_text())
    except (OSError, json.JSONDecodeError):
        return []
    hosts = data.get("hosts", []) if isinstance(data, dict) else []
    out: list[dict] = []
    for h in hosts:
        if not isinstance(h, dict) or not h.get("addr"):
            continue
        out.append({
            "name": str(h.get("name") or h.get("addr", "")),
            "addr": str(h.get("addr", "")),
            "port": int(h.get("port", 9777) or 9777),
            "fp_hex": str(h.get("fp_hex", "")),
            "paired": bool(h.get("paired", False)),
            "mac": h.get("mac") if isinstance(h.get("mac"), list) else [],
            "last_used": h.get("last_used"),
            "online": None,
        })
    return out


def _mutation_result(rc: int, err: str, op: str) -> dict:
    """Map a headless host-store mutation's exit status to a UI-stable result. ``rc == -1`` means
    the flatpak call never ran (missing/timed out); a nonzero rc from a client that PREDATES the
    mode falls through to GTK init and fails headless — classified ``client-outdated`` so the UI
    can prompt an update instead of showing a cryptic error."""
    if rc == 0:
        return {"ok": True}
    if rc == -1:
        return {"ok": False, "error": "client-unavailable"}
    code = _classify_library_error(err)
    detail = (err.strip().splitlines() or [f"{op} failed"])[-1]
    decky.logger.warning("%s failed (rc=%s): %s", op, rc, detail)
    return {"ok": False, "error": code, "detail": detail}


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

        try:
            mgmt = int(props.get("mgmt", ""))
        except ValueError:
            mgmt = 0  # not advertised (standalone punktfunk1-host) — callers default 47990

        entry = {
            "name": name,
            "host": address,
            "port": port,
            "pair": props.get("pair", "optional"),
            "fp": props.get("fp", ""),
            "proto": props.get("proto", ""),
            "id": props.get("id", ""),
            "mgmt": mgmt,
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
            _invalidate_hosts_cache()  # the store gained a paired entry — reflect it next list
            return {"ok": True, "fp": fp}
        decky.logger.warning("pairing failed (rc=%s): %s", proc.returncode, err.strip() or out.strip())
        # Surface the client's own one-line reason (wrong PIN / not armed) to the UI.
        reason = (err.strip().splitlines() or out.strip().splitlines() or ["pairing failed"])[-1]
        return {"ok": False, "error": reason}

    async def wake(self, host: str, port: int = 9777) -> dict:
        """Send a Wake-on-LAN magic packet to a saved host via the flatpak client's headless
        ``--wake`` mode, so a sleeping host is up by the time the stream ``--connect`` runs.

        The MAC comes from the flatpak client's OWN known-hosts store (learned from the host's
        mDNS ``mac`` TXT while it was online) — no MAC handling here — so this is a no-op if none
        has been learned yet. Fire it just before launching a stream; it's fast and best-effort.
        Returns ``{ok, error?}`` (``ok: False`` when no MAC is known / flatpak missing).
        """
        flatpak = _flatpak()
        if not flatpak:
            return {"ok": False, "error": "flatpak-not-found"}
        argv = [flatpak, "run", "--arch=x86_64", APP_ID, "--wake", f"{host}:{port}"]
        decky.logger.info("wake: %s:%s", host, port)
        try:
            proc = await asyncio.create_subprocess_exec(
                *argv,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                env=_flatpak_env(),
            )
            _, stderr = await asyncio.wait_for(proc.communicate(), timeout=15.0)
        except asyncio.TimeoutError:
            return {"ok": False, "error": "wake timed out"}
        except Exception as exc:  # noqa: BLE001
            decky.logger.exception("wake failed to launch")
            return {"ok": False, "error": str(exc)}
        if proc.returncode == 0:
            return {"ok": True}
        reason = (stderr.decode(errors="replace").strip().splitlines() or
                  ["no MAC known for this host yet"])[-1]
        decky.logger.info("wake skipped (rc=%s): %s", proc.returncode, reason)
        return {"ok": False, "error": reason}

    async def library(self, host: str, mgmt_port: int = 0, fp: str = "") -> dict:
        """Fetch a paired host's game library via the flatpak client's headless
        ``--library`` mode (the client's own mTLS identity + pinned-fingerprint transport —
        no trust logic reimplemented here). ``fp`` is passed through whenever the caller
        knows the host's cert fingerprint so an IP change can never degrade the pin to a
        TOFU accept. Returns ``{ok, games: [{id, store, title}]}`` or
        ``{ok: False, error: <code>, detail}`` (codes: ``flatpak-not-found`` / ``timeout`` /
        ``not-paired`` / ``pin-mismatch`` / ``unreachable`` / ``http`` /
        ``client-outdated`` / ``client-error``)."""
        flatpak = _flatpak()
        if not flatpak:
            return {"ok": False, "error": "flatpak-not-found", "detail": ""}
        target = f"{host}:{int(mgmt_port) or 47990}"
        argv = [flatpak, "run", "--arch=x86_64", APP_ID, "--library", target]
        if fp:
            argv += ["--fp", fp]
        decky.logger.info("library: fetching %s", target)
        proc = None
        try:
            # Separate pipes (unlike _flatpak_capture): the TSV comes on stdout, the
            # client's one-line error reason on stderr. Cold flatpak start on a Deck can
            # take seconds — generous timeout, spinner in the UI.
            proc = await asyncio.create_subprocess_exec(
                *argv,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                env=_flatpak_env(),
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=45.0)
        except asyncio.TimeoutError:
            if proc:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
            return {"ok": False, "error": "timeout", "detail": ""}
        except Exception as exc:  # noqa: BLE001
            decky.logger.exception("library fetch failed to launch")
            return {"ok": False, "error": "client-error", "detail": str(exc)}

        err = stderr.decode(errors="replace")
        if proc.returncode != 0:
            detail = (err.strip().splitlines() or ["library fetch failed"])[-1]
            code = _classify_library_error(err)
            decky.logger.warning("library fetch failed (%s): %s", code, detail)
            return {"ok": False, "error": code, "detail": detail}
        games = _parse_library_tsv(stdout.decode(errors="replace"))
        decky.logger.info("library: %d game(s) from %s", len(games), target)
        return {"ok": True, "games": games}

    async def get_pins(self) -> dict:
        """The pinned games, each annotated with the LIVE ``paired`` state of its host (by
        cert fingerprint — an unpaired-since host renders "pairing required" in the QAM)."""
        try:
            data = json.loads(_pins_path().read_text())
        except (OSError, json.JSONDecodeError):
            return {"pins": []}
        pins = data.get("pins", []) if isinstance(data, dict) else []
        paired = _paired_fingerprints()
        out = []
        for p in pins:
            if not isinstance(p, dict) or not p.get("game_id"):
                continue
            p = dict(p)
            p["paired"] = str(p.get("host_fp", "")).lower() in paired
            out.append(p)
        return {"pins": out}

    async def set_pins(self, pins: list) -> dict:
        """Persist the pinned-games list (the frontend sends the whole list — add, remove,
        and address-refresh all funnel through here). Validated + deduped on
        ``(host_fp, game_id)``; written atomically (tmp + rename) — pins are long-lived
        user data."""
        clean: list[dict] = []
        seen: set[tuple[str, str]] = set()
        for p in pins if isinstance(pins, list) else []:
            if not isinstance(p, dict):
                continue
            game_id = str(p.get("game_id", ""))
            host_fp = str(p.get("host_fp", ""))
            if not game_id or not (host_fp or p.get("host")):
                continue
            key = (host_fp, game_id)
            if key in seen:
                continue
            seen.add(key)
            clean.append({
                "game_id": game_id,
                "title": str(p.get("title", game_id)),
                "store": str(p.get("store", "")),
                "host_fp": host_fp,
                "host_id": str(p.get("host_id", "")),
                "host_name": str(p.get("host_name", p.get("host", ""))),
                "host": str(p.get("host", "")),
                "port": int(p.get("port", 9777) or 9777),
                "mgmt": int(p.get("mgmt", 0) or 0),
                "added_at": int(p.get("added_at", 0) or 0),
            })
        try:
            d = _client_config_dir()
            d.mkdir(parents=True, exist_ok=True)
            tmp = _pins_path().with_suffix(".json.tmp")
            tmp.write_text(json.dumps({"version": 1, "pins": clean}, indent=2))
            os.replace(tmp, _pins_path())
            return {"ok": True}
        except OSError as exc:
            decky.logger.exception("could not write pins")
            return {"ok": False, "error": str(exc)}

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
                "codec": "auto", "gamepad": "auto", "compositor": "auto",
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

    # ---- Shared known-hosts store (the SAME file the desktop client reads/writes) ----

    async def list_hosts(self, probe: bool = True) -> dict:
        """The saved-hosts store as a list — the SAME ``client-known-hosts.json`` the desktop
        client owns, so a host added/renamed/paired in either surface shows in both. With
        ``probe`` each host carries a live ``online`` bool from a mDNS-INDEPENDENT reachability
        probe (a Tailscale/VPN host is no longer shown offline just because it doesn't advertise);
        ``online`` is ``None`` when reachability is unknown. Prefers the client's ``--list-hosts``
        mode; falls back to reading the JSON directly when the installed client predates it."""
        now = time.monotonic()
        cache = _hosts_cache
        if (
            cache["data"] is not None
            and cache["probed"] == bool(probe)
            and (now - cache["at"]) < _HOSTS_TTL_S
        ):
            return cache["data"]

        args = ["--list-hosts"] + (["--probe"] if probe else [])
        rc, out, err = await _run_client(args, timeout=30.0)
        result: dict | None = None
        if rc == 0:
            try:
                data = json.loads(out)
                hosts = data.get("hosts", []) if isinstance(data, dict) else []
                result = {"ok": True, "hosts": hosts, "probed": bool(probe)}
            except json.JSONDecodeError:
                decky.logger.warning("list-hosts: unparseable output: %s", out[:200])
        elif rc != -1:
            decky.logger.info(
                "list-hosts unavailable (%s); reading store directly",
                _classify_library_error(err),
            )
        if result is None:
            # Fallback: read the store off disk (old client / no --list-hosts) — no reachability.
            result = {"ok": True, "hosts": _read_known_hosts(), "probed": False, "fallback": True}
        cache.update(at=now, probed=bool(probe), data=result)
        return result

    async def add_host(self, target: str, name: str = "", fp: str = "") -> dict:
        """Save a host by address so it can be paired/streamed even when mDNS never sees it (a
        Tailscale/VPN box). Without ``fp`` it's an unpaired placeholder the user pairs next; a
        later pair replaces it with the fingerprinted entry. Returns ``{ok, error?, detail?}``."""
        args = ["--add-host", target.strip()]
        if name.strip():
            args += ["--host-label", name.strip()]
        if fp.strip():
            args += ["--fp", fp.strip()]
        rc, _out, err = await _run_client(args, timeout=20.0)
        _invalidate_hosts_cache()
        return _mutation_result(rc, err, "add-host")

    async def edit_host(
        self, selector: str, name: str = "", addr: str = "", port: int = 0
    ) -> dict:
        """Edit a saved host — rename and/or re-point its address. ``selector`` is the host's
        cert fingerprint (survives IP changes) or its current ``addr[:port]``. Empty fields are
        left untouched. Returns ``{ok, error?, detail?}``."""
        args = ["--set-host", selector]
        if name.strip():
            args += ["--host-label", name.strip()]
        if addr.strip():
            args += ["--addr", addr.strip()]
        if port:
            args += ["--port", str(int(port))]
        rc, _out, err = await _run_client(args, timeout=20.0)
        _invalidate_hosts_cache()
        return _mutation_result(rc, err, "set-host")

    async def forget_host(self, selector: str) -> dict:
        """Remove a saved host (by fingerprint or ``addr[:port]``) — drops the pinned
        fingerprint, so a later connect must re-pair/trust. Idempotent."""
        rc, _out, err = await _run_client(["--forget-host", selector], timeout=20.0)
        _invalidate_hosts_cache()
        return _mutation_result(rc, err, "forget-host")

    async def reset_config(self) -> dict:
        """Reset this device's Punktfunk state: saved hosts, stream settings, and the plugin's
        pinned games. The client's persistent IDENTITY (client-cert/key.pem) is KEPT so the box
        isn't seen as brand-new everywhere (re-pairing re-adds hosts). Prefers the client's
        ``--reset``; if that's unavailable (old client / no flatpak) it clears the shared JSON
        stores directly. The plugin-owned pins file is always cleared here (``--reset`` never
        touches it)."""
        rc, _out, _err = await _run_client(["--reset"], timeout=20.0)
        _invalidate_hosts_cache()
        errors: list[str] = []
        if rc != 0:
            decky.logger.info("reset: --reset unavailable (rc=%s); clearing stores directly", rc)
            for name in ("client-known-hosts.json", "client-gtk-settings.json"):
                try:
                    (_client_config_dir() / name).unlink()
                except FileNotFoundError:
                    pass
                except OSError as exc:
                    errors.append(f"{name}: {exc}")
        try:
            _pins_path().unlink()  # plugin-owned; the client's --reset leaves it alone
        except FileNotFoundError:
            pass
        except OSError as exc:
            errors.append(f"pins: {exc}")
        if errors:
            return {"ok": False, "error": "; ".join(errors)}
        return {"ok": True}

    async def probe_host(self, target: str) -> dict:
        """Reachability of one ``host[:port]`` via the client's mDNS-independent QUIC probe —
        for a "test this address" check. ``{ok: True, online: bool}`` when determined, else
        ``{ok: False, error}`` (flatpak missing / client too old)."""
        rc, _out, err = await _run_client(["--reachable", target.strip()], timeout=8.0)
        if rc == 0:
            return {"ok": True, "online": True}
        if rc == 1:
            return {"ok": True, "online": False}
        return {
            "ok": False,
            "error": _classify_library_error(err) if err.strip() else "client-unavailable",
        }

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
