// Shared state hooks + user actions for the QAM panel and the fullscreen page.
import { toaster } from "@decky/api";
import { Navigation } from "@decky/ui";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  checkUpdate,
  discover,
  GameEntry,
  getPins,
  Host,
  listHosts,
  PinnedGame,
  resetConfig,
  SavedHost,
  setPins as setPinsBackend,
  updateClient,
  UpdateInfo,
} from "./backend";
import { LaunchOpts, launchStream } from "./steam";

export const DOCS_URL = "https://docs.punktfunk.unom.io/docs/steam-deck";

// Decky Loader exposes its already-authenticated WSRouter as a global. This is NOT part of
// @decky/api (it's a loader internal), so we treat it as optional and guard every use — on a
// loader without it we fall back to manual "Install Plugin from URL". We use it to drive
// Decky's own privileged install path (the root loader does the download + SHA-256 verify +
// extract + hot-reload), which is the only way a plugin can update itself: ~/homebrew/plugins
// is root-owned, so our unprivileged backend can't swap its own files.
declare global {
  interface Window {
    DeckyBackend?: {
      callable: (route: string) => (...args: unknown[]) => Promise<unknown>;
    };
  }
}

// PluginInstallType.UPDATE in decky-loader's browser.py (INSTALL=0/REINSTALL=1/UPDATE=2/…).
const INSTALL_TYPE_UPDATE = 2;

// ----------------------------------------------------------------------------------------
// Discovery — mDNS scan state shared by the QAM panel and the full page.
// ----------------------------------------------------------------------------------------
export function useHosts() {
  const [hosts, setHosts] = useState<Host[]>([]);
  const [scanning, setScanning] = useState(false);

  const refresh = useCallback(async () => {
    setScanning(true);
    try {
      setHosts(await discover());
    } catch (e) {
      toaster.toast({ title: "Punktfunk", body: `Discovery failed: ${e}` });
    } finally {
      setScanning(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { hosts, scanning, refresh };
}

// ----------------------------------------------------------------------------------------
// Saved hosts — the SHARED known-hosts store (client-known-hosts.json), the same file the
// desktop client reads/writes. Fetched WITH a reachability probe so a host reached over a
// routed network (Tailscale/VPN) reports online without ever appearing on mDNS.
// ----------------------------------------------------------------------------------------
export function useSavedHosts() {
  const [saved, setSaved] = useState<SavedHost[]>([]);
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const r = await listHosts(true);
      setSaved(r.hosts ?? []);
    } catch {
      /* backend unavailable — keep the current view */
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { saved, loading, refresh };
}

/**
 * One host as the UI shows it — the union of the saved store and the live mDNS scan. A saved
 * host is ONLINE when it either advertises on mDNS OR answers the reachability probe (so
 * mDNS-blind-but-reachable hosts stop reading as offline). Discovered hosts not in the store
 * are appended as unsaved rows.
 */
export interface HostView {
  name: string;
  addr: string;
  port: number;
  fp: string; // "" for a saved-but-unpaired placeholder
  paired: boolean; // PIN-paired specifically (a TOFU host has fp but paired=false)
  online: boolean;
  saved: boolean; // present in the known-hosts store
  pairPolicy: string; // the advert's policy ("required"|"optional"), "" when not advertising
  mgmt: number; // advertised mgmt-API port (0 = not advertised → default)
  id: string; // advertised stable host id ("" when not advertising)
  os: string; // OS-identity chain (live advert preferred, else the stored one); "" unknown
}

function advertMatchesSaved(a: Host, s: SavedHost): boolean {
  return (
    (!!s.fp_hex && !!a.fp && s.fp_hex.toLowerCase() === a.fp.toLowerCase()) ||
    (s.addr === a.host && s.port === a.port)
  );
}

export function mergeHosts(saved: SavedHost[], discovered: Host[]): HostView[] {
  const views: HostView[] = saved.map((s) => {
    // Prefer a live advert's address (a host may have moved DHCP leases since it was saved).
    const advert = discovered.find((a) => advertMatchesSaved(a, s));
    return {
      name: s.name || s.addr,
      addr: advert?.host ?? s.addr,
      port: advert?.port ?? s.port,
      fp: s.fp_hex || advert?.fp || "",
      paired: s.paired,
      online: !!advert || s.online === true,
      saved: true,
      pairPolicy: advert?.pair ?? "",
      mgmt: advert?.mgmt ?? 0,
      id: advert?.id ?? "",
      os: advert?.os || s.os || "",
    };
  });
  for (const a of discovered) {
    if (saved.some((s) => advertMatchesSaved(a, s))) {
      continue; // already rendered as its saved card (with a live pip)
    }
    views.push({
      name: a.name,
      addr: a.host,
      port: a.port,
      fp: a.fp,
      paired: a.paired,
      online: true,
      saved: false,
      pairPolicy: a.pair,
      mgmt: a.mgmt,
      id: a.id,
      os: a.os,
    });
  }
  return views;
}

/**
 * True when this host must be paired before it can stream. A saved host is streamable once it
 * has a pinned fingerprint (PIN-paired OR TOFU-trusted); a saved placeholder (no fp yet) must be
 * paired. For an unsaved discovered host we keep the advertised-policy rule the UI always used.
 */
export function needsPair(v: HostView): boolean {
  return v.saved ? v.fp === "" : v.pairPolicy === "required" && !v.paired;
}

/** Adapt a merged view back into the `Host` shape the pair/library/stream helpers consume. */
export function toHost(v: HostView): Host {
  return {
    name: v.name,
    host: v.addr,
    port: v.port,
    pair: v.pairPolicy || (needsPair(v) ? "required" : "optional"),
    fp: v.fp,
    proto: "",
    paired: v.paired,
    id: v.id,
    mgmt: v.mgmt,
    os: v.os,
  };
}

/** Is a pinned game's host currently online, considering BOTH the live scan and saved probe? */
export function pinIsOnline(pin: PinnedGame, views: HostView[]): boolean {
  const fp = pin.host_fp.toLowerCase();
  return views.some(
    (v) =>
      v.online &&
      ((!!fp && v.fp.toLowerCase() === fp) ||
        (!!pin.host_id && v.id === pin.host_id) ||
        (v.addr === pin.host && v.port === pin.port)),
  );
}

/**
 * Reset all Punktfunk state (saved hosts + stream settings + pins), keeping the client identity.
 * Refreshes whatever views are passed so the UI clears immediately. Ends in a toast.
 */
export async function resetAll(refreshers: Array<() => void | Promise<void>>): Promise<void> {
  try {
    const r = await resetConfig();
    for (const fn of refreshers) void fn();
    toaster.toast({
      title: "Punktfunk",
      body: r.ok
        ? "Reset — saved hosts, settings, and pins cleared."
        : `Reset failed${r.error ? ` (${r.error})` : ""}.`,
    });
  } catch {
    toaster.toast({ title: "Punktfunk", body: "Reset failed." });
  }
}

// ----------------------------------------------------------------------------------------
// Self-update — checks our registry on mount (the backend caches for 30 min + is non-fatal
// offline); `check(true)` bypasses the cache for the explicit "Check for updates" button.
// ----------------------------------------------------------------------------------------
export function useUpdate() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);

  const check = useCallback(async (force: boolean): Promise<UpdateInfo | null> => {
    setChecking(true);
    try {
      const res = await checkUpdate(force);
      setInfo(res);
      return res;
    } catch {
      return null;
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    void check(false);
  }, [check]);

  return { info, checking, check };
}

/** True when EITHER the plugin or the flatpak client has a pending update. */
export function hasUpdate(info: UpdateInfo | null | undefined): boolean {
  return !!info && (info.update_available || info.client_update_available);
}

/** The explicit "Check for updates" action — always ends in a toast so the tap has feedback. */
export async function checkForUpdatesNow(
  check: (force: boolean) => Promise<UpdateInfo | null>,
): Promise<void> {
  const res = await check(true);
  let body: string;
  if (!res || res.error === "fetch-failed") {
    body = "Couldn’t reach the update server — are you online?";
  } else if (hasUpdate(res)) {
    const parts: string[] = [];
    if (res.update_available) parts.push(`plugin v${res.current} → v${res.latest}`);
    if (res.client_update_available) parts.push("client");
    body = `Update available: ${parts.join(" + ")}.`;
  } else if (res.error === "update-channel-unknown") {
    body = "Development build — plugin updates are disabled; the client is up to date.";
  } else {
    body = `You’re up to date (plugin v${res.current}).`;
  }
  toaster.toast({ title: "Punktfunk", body });
}

/**
 * Apply whichever updates are pending. The flatpak CLIENT is updated first (a user-scope
 * `flatpak update`, awaited); then, if the PLUGIN itself has an update, Decky's install RPC
 * reinstalls it — which reloads the plugin and tears this panel down, so it goes last and is
 * fire-and-forget. `check` (when passed) refreshes the panel state after a client-only update so
 * the "Update available" button clears.
 */
export async function applyUpdate(
  info: UpdateInfo,
  check?: (force: boolean) => Promise<UpdateInfo | null>,
): Promise<void> {
  if (info.client_update_available) {
    toaster.toast({ title: "Punktfunk", body: "Updating the client…" });
    try {
      const r = await updateClient();
      toaster.toast({
        title: "Punktfunk",
        body: !r.ok
          ? `Client update failed${r.error ? ` (${r.error})` : ""}.`
          : r.updated
            ? "Client updated to the latest version."
            : "Client is already up to date.",
      });
    } catch {
      toaster.toast({ title: "Punktfunk", body: "Client update failed." });
    }
  }

  if (info.update_available) {
    try {
      const backend = window.DeckyBackend;
      if (backend?.callable) {
        // Fire-and-forget: the loader reinstalls + reloads THIS plugin, tearing the panel down
        // before any result could arrive — so never await it. Decky shows its own confirm prompt.
        void backend.callable("utilities/install_plugin")(
          info.artifact,
          "punktfunk",
          info.latest,
          info.hash,
          INSTALL_TYPE_UPDATE,
        );
        toaster.toast({
          title: "Punktfunk",
          // Decky's installer also phones the plugin store first, which can hang on some
          // networks before the actual install proceeds — set expectations.
          body: `Updating the plugin to v${info.latest} — confirm Decky’s prompt. This can take a couple of minutes.`,
        });
        return;
      }
    } catch {
      // fall through to the manual path
    }
    toaster.toast({
      title: "Punktfunk",
      body: "Update the plugin from Decky → Developer → Install Plugin from URL.",
    });
    return;
  }

  // Client-only update (no plugin reinstall): refresh so the button clears.
  if (check) void check(true);
}

// ----------------------------------------------------------------------------------------
// Stream launch — via the hidden Steam shortcut (see steam.ts for why).
// ----------------------------------------------------------------------------------------
export async function startStream(
  h: Host,
  opts: LaunchOpts = {},
  label?: string,
): Promise<void> {
  try {
    await launchStream(h.host, h.port, opts);
    Navigation.CloseSideMenus();
    toaster.toast({ title: "Punktfunk", body: `Starting ${label ?? "stream"} — ${h.name}` });
  } catch (e) {
    toaster.toast({ title: "Punktfunk", body: `Launch failed: ${e}` });
  }
}

/** Open the GTK client's gamepad library launcher for a host (`--browse` via PF_BROWSE). */
export async function startBrowse(h: Host): Promise<void> {
  try {
    await launchStream(h.host, h.port, { browse: true, mgmt: h.mgmt });
    Navigation.CloseSideMenus();
    toaster.toast({ title: "Punktfunk", body: `Opening library — ${h.name}` });
  } catch (e) {
    toaster.toast({ title: "Punktfunk", body: `Launch failed: ${e}` });
  }
}

// ----------------------------------------------------------------------------------------
// Pinned games — the QAM's one-tap game rows, persisted by the backend next to the
// client's config (survives plugin reinstalls).
// ----------------------------------------------------------------------------------------
export interface PinsApi {
  pins: PinnedGame[];
  addPin: (h: Host, g: GameEntry) => void;
  removePin: (hostFp: string, gameId: string) => void;
  isPinned: (hostFp: string, gameId: string) => boolean;
  /** Refresh a pin's stored address from a live advert (hosts change IPs). */
  updatePinHost: (pin: PinnedGame, h: Host) => void;
  refresh: () => Promise<void>;
}

export function usePins(): PinsApi {
  const [pins, setPins] = useState<PinnedGame[]>([]);
  // A live mirror of `pins`. The Games picker is mounted by Decky's `showModal` into a
  // detached portal that captures this hook's callbacks ONCE and never re-renders with fresh
  // props, so a mutator closing over the `pins` array reads a frozen base — pinning a second
  // game in the same session would compute from the stale `[]` and clobber the first (silent
  // data loss). Reading the ref keeps every mutation based on the current set, and lets the
  // callbacks keep a stable identity (deps free of `pins`).
  const pinsRef = useRef<PinnedGame[]>([]);
  pinsRef.current = pins;

  const refresh = useCallback(async () => {
    try {
      setPins((await getPins()).pins);
    } catch {
      /* backend unavailable — keep the current view */
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Optimistic local state; the backend validates/dedups and is re-read on failure.
  const save = useCallback(
    (next: PinnedGame[]) => {
      pinsRef.current = next;
      setPins(next);
      setPinsBackend(next).catch(() => void refresh());
    },
    [refresh],
  );

  const addPin = useCallback(
    (h: Host, g: GameEntry) => {
      const pin: PinnedGame = {
        game_id: g.id,
        title: g.title,
        store: g.store,
        host_fp: h.fp,
        host_id: h.id,
        host_name: h.name,
        host: h.host,
        port: h.port,
        mgmt: h.mgmt,
        added_at: Math.floor(Date.now() / 1000),
        paired: h.paired,
      };
      save([
        ...pinsRef.current.filter(
          (p) => !(p.host_fp === pin.host_fp && p.game_id === pin.game_id),
        ),
        pin,
      ]);
    },
    [save],
  );

  const removePin = useCallback(
    (hostFp: string, gameId: string) => {
      save(pinsRef.current.filter((p) => !(p.host_fp === hostFp && p.game_id === gameId)));
    },
    [save],
  );

  const isPinned = useCallback(
    (hostFp: string, gameId: string) =>
      pins.some((p) => p.host_fp === hostFp && p.game_id === gameId),
    [pins],
  );

  const updatePinHost = useCallback(
    (pin: PinnedGame, h: Host) => {
      if (pin.host === h.host && pin.port === h.port && pin.mgmt === h.mgmt) {
        return;
      }
      save(
        pinsRef.current.map((p) =>
          p.host_fp === pin.host_fp && p.game_id === pin.game_id
            ? { ...p, host: h.host, port: h.port, mgmt: h.mgmt, host_name: h.name }
            : p,
        ),
      );
    },
    [save],
  );

  return { pins, addPin, removePin, isPinned, updatePinHost, refresh };
}

/**
 * The host a pin should launch against right now: match the live mDNS scan by cert
 * fingerprint first (pairing is fp-keyed, survives IP changes), then by the host's stable
 * id, else fall back to the stored address (host offline or scan flaky — still launch).
 */
export function resolvePinHost(
  pin: PinnedGame,
  live: Host[],
): { host: Host; online: boolean } {
  const fp = pin.host_fp.toLowerCase();
  const match =
    (fp && live.find((h) => h.fp && h.fp.toLowerCase() === fp)) ||
    (pin.host_id && live.find((h) => h.id && h.id === pin.host_id)) ||
    undefined;
  if (match) {
    return { host: match, online: true };
  }
  return {
    host: {
      name: pin.host_name || pin.host,
      host: pin.host,
      port: pin.port,
      pair: pin.paired ? "optional" : "required",
      fp: pin.host_fp,
      proto: "",
      paired: !!pin.paired,
      id: pin.host_id,
      mgmt: pin.mgmt,
      os: "", // pins don't store the chain; the icon is a hosts-tab affordance
    },
    online: false,
  };
}
