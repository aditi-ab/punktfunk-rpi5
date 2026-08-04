// Shared state hooks + user actions for the QAM panel.
import { toaster } from "@decky/api";
import { Navigation } from "@decky/ui";
import { useCallback, useEffect, useState } from "react";
import {
  checkUpdate,
  discover,
  DiscoveredHost,
  hosts as listHosts,
  Profile,
  SavedHost,
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

/**
 * How far this device has got with a host. The three states are what the row says under the
 * name, and which of them a host is in decides whether pressing it streams or opens the trust
 * sheet.
 *
 * - `paired`       — the host approved this device (a PIN ceremony, or request access).
 * - `trusted`      — its fingerprint is pinned but nobody has approved us yet. Streams work if
 *                    the host's policy is `optional`; under `required` the connect parks.
 * - `needs-access` — no pinned fingerprint. Not streamable until the trust sheet runs.
 */
export type TrustState = "paired" | "trusted" | "needs-access";

/**
 * One host as the panel shows it — the union of the saved store and the live mDNS browse.
 *
 * A saved host is ONLINE when it either advertises or answers the reachability probe, so a box
 * reached over Tailscale/VPN stops reading as offline. Discovered hosts that aren't saved are
 * appended as extra rows.
 */
export interface HostView {
  name: string;
  addr: string;
  port: number;
  /**
   * The fingerprint PINNED ON THE RECORD. "" means nothing is pinned, which is exactly what
   * makes a host unstreamable — the session binary refuses a pinless connect.
   *
   * Deliberately NOT filled in from a live advert. A host saved by address that happens to be
   * advertising right now still has an empty pin on disk, and borrowing the advert's here would
   * draw it as ready to stream while every launch refused for want of a fingerprint. What the
   * advert offers is [`advertisedFp`], and moving it onto the record is a trust decision the
   * user makes in the sheet.
   */
  fp: string;
  /** What the host is advertising right now, if anything — what request access would pin. */
  advertisedFp: string;
  /**
   * The host is answering at an address its record does not carry — it changed DHCP lease.
   *
   * This matters because a launch names the host by [`ref`], and the CLI dials whatever address
   * the RECORD holds. So the row would show the live address and dial the dead one. The record
   * has to be re-pointed before such a host can stream; `startStream` does it.
   */
  moved: boolean;
  paired: boolean;
  online: boolean;
  saved: boolean;
  /** The advert's policy ("required"|"optional"); "" when the host isn't advertising. */
  pairPolicy: string;
  /** OS-identity chain (live advert preferred, else the stored one); "" unknown. */
  os: string;
  /**
   * What a launch should NAME this host by: the record's stable id, which survives renames and
   * DHCP moves, falling back to `addr:port` for a row that has no record yet (a discovered host
   * the trust sheet is about to save, or a client too old to have minted ids).
   */
  ref: string;
  /** The host's default profile binding — applied silently by a plain connect, not a card. */
  profile: Profile | null;
  /** The cards to render nested under this host; already resolved against the catalog. */
  pinnedProfiles: Profile[];
  lastUsed: number | null;
}

export function trustState(v: HostView): TrustState {
  if (v.paired) return "paired";
  return v.fp ? "trusted" : "needs-access";
}

/**
 * Must this host go through the trust sheet before it can stream?
 *
 * A pinned fingerprint is the ONLY rule. The session binary refuses a pinless connect, so a row
 * without one can offer nothing but a button that fails; with one, the connect is verified and
 * the host either admits it or parks it for an operator. The old rule also consulted the
 * advertised policy for unsaved hosts, which made the answer depend on which of two lists a row
 * came from — the same box could read differently before and after being saved.
 */
export function needsPair(v: HostView): boolean {
  return v.fp === "";
}

function advertMatchesSaved(a: DiscoveredHost, s: SavedHost): boolean {
  return (
    (!!s.fp_hex && !!a.fp && s.fp_hex.toLowerCase() === a.fp.toLowerCase()) ||
    (s.addr === a.addr && s.port === a.port)
  );
}

/**
 * Join the saved store and the live browse into the rows the panel draws.
 *
 * Fingerprint first, address second — a host that moved DHCP lease still matches its record,
 * and a different box that inherited the old address does not inherit its pairing. The CLI's
 * `discover` annotates `saved`/`paired` by exactly this rule too, so the two can't disagree.
 */
export function mergeHosts(saved: SavedHost[], discovered: DiscoveredHost[]): HostView[] {
  const views: HostView[] = saved.map((s) => {
    // Prefer a live advert's address: the host may have moved since it was last saved.
    const advert = discovered.find((a) => advertMatchesSaved(a, s));
    return {
      name: s.name || s.addr,
      addr: advert?.addr ?? s.addr,
      port: advert?.port ?? s.port,
      fp: s.fp_hex,
      advertisedFp: advert?.fp ?? "",
      moved: !!advert && (advert.addr !== s.addr || advert.port !== s.port),
      paired: s.paired,
      online: !!advert || s.online === true,
      saved: true,
      pairPolicy: advert?.pair ?? "",
      os: advert?.os || s.os || "",
      ref: s.id || `${advert?.addr ?? s.addr}:${advert?.port ?? s.port}`,
      profile: s.profile,
      pinnedProfiles: s.pinned_profiles ?? [],
      lastUsed: s.last_used,
    };
  });
  for (const a of discovered) {
    if (saved.some((s) => advertMatchesSaved(a, s))) {
      continue; // already rendered as its saved row, with a live pip
    }
    views.push({
      name: a.name,
      addr: a.addr,
      port: a.port,
      // No record, so nothing is pinned — whatever it advertises is an OFFER, not a pin.
      fp: "",
      advertisedFp: a.fp,
      moved: false, // no record, so nothing to be stale
      paired: a.paired,
      online: true,
      saved: false,
      pairPolicy: a.pair,
      os: a.os,
      ref: `${a.addr}:${a.port}`,
      profile: null,
      pinnedProfiles: [],
      lastUsed: null,
    });
  }
  return views.sort(sortRows);
}

/**
 * Online first, then most recently used, then by name. The host you streamed last night should
 * be the first thing under your thumb; a host that is off right now should never be.
 */
function sortRows(a: HostView, b: HostView): number {
  if (a.online !== b.online) return a.online ? -1 : 1;
  if ((a.lastUsed ?? 0) !== (b.lastUsed ?? 0)) return (b.lastUsed ?? 0) - (a.lastUsed ?? 0);
  return a.name.localeCompare(b.name);
}

// ----------------------------------------------------------------------------------------
// Hosts — ONE call site for both lists. They were separate hooks when the plugin had two
// views mounting them independently; the panel is the only view now, and merging them means
// the "scanning" state covers the whole row set rather than half of it flickering in first.
// ----------------------------------------------------------------------------------------
export function useHosts() {
  const [views, setViews] = useState<HostView[]>([]);
  const [scanning, setScanning] = useState(false);
  // Why the list is empty, when it is empty for a reason other than an empty LAN. Rendering
  // either of these as "No hosts yet" would blame the user's network for the plugin's problem:
  //   "client-outdated"    — the installed client predates `punktfunk discover`
  //   "client-unavailable" — there is no client installed at all
  const [problem, setProblem] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setScanning(true);
    try {
      // Both in flight at once: the browse is time-bounded and the probe is network-bound, so
      // running them in sequence would cost the sum of two waits for no benefit.
      const [d, s] = await Promise.all([discover(), listHosts()]);
      // Both calls run the same binary, so they fail the same way; take whichever answered.
      setProblem(
        d.error === "client-unavailable" || s.error === "client-unavailable"
          ? "client-unavailable"
          : d.error === "client-outdated" || s.error === "client-outdated"
            ? "client-outdated"
            : null,
      );
      setViews(mergeHosts(s.hosts ?? [], d.hosts ?? []));
    } catch (e) {
      toaster.toast({ title: "Punktfunk", body: `Couldn't list hosts: ${e}` });
    } finally {
      setScanning(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { views, scanning, problem, refresh };
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

/** True when EITHER the plugin or the client has a pending update. */
export function hasUpdate(info: UpdateInfo | null | undefined): boolean {
  return !!info && (info.update_available || info.client_update_available);
}

/**
 * Can this Deck actually INSTALL the pending client update, or only tell you how?
 *
 * A flatpak and a one-tap-capable native install (the packaged root helper + the operator's
 * group opt-in) get a button; a sysext, a nix profile, a source build or a box that hasn't
 * opted in gets the command. Offering a button that can only fail is worse than saying so.
 */
export function clientUpdateIsOneTap(info: UpdateInfo | null | undefined): boolean {
  return (
    !!info &&
    info.client_update_available &&
    (info.client_applier === "flatpak" || info.client_applier === "helper")
  );
}

/** True when the only pending update is one this Deck can't apply itself. */
export function clientUpdateIsManualOnly(info: UpdateInfo | null | undefined): boolean {
  return !!info && info.client_update_available && !clientUpdateIsOneTap(info);
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
    if (res.client_update_available) {
      parts.push(res.client_latest ? `client ${res.client_latest}` : "client");
    }
    body = `Update available: ${parts.join(" + ")}.`;
    if (clientUpdateIsManualOnly(res)) {
      // Say the honest thing up front rather than letting the user find out at the button.
      body += " The client updates outside Punktfunk on this install.";
    }
  } else if (res.client_error) {
    // A failed CLIENT check must never read as "up to date" — that is the one wrong answer.
    body =
      res.client_error === "client-outdated"
        ? "Couldn’t check the client — it predates update checks. Update it once by hand."
        : "Couldn’t check the client for updates.";
  } else if (res.error === "update-channel-unknown") {
    body = "Development build — plugin updates are disabled; the client is up to date.";
  } else {
    body = `You’re up to date (plugin v${res.current}).`;
  }
  toaster.toast({ title: "Punktfunk", body });
}

/** One line of user-facing copy for whatever `updateClient()` came back with. */
function clientUpdateResultBody(r: Awaited<ReturnType<typeof updateClient>>): string {
  if (r.ok) {
    if (r.staged) return "Client updated — reboot to finish.";
    return r.updated ? "Client updated to the latest version." : "Client is already up to date.";
  }
  // "manual" is not a failure: the box simply can't install it, and `command` says how.
  if (r.error === "manual") {
    return r.command
      ? `This client updates outside Punktfunk. Run: ${r.command}`
      : "This client updates outside Punktfunk — use the way you installed it.";
  }
  if (r.error === "timeout") return "Client update timed out — check the box and try again.";
  if (r.error === "client-unavailable")
    return "Couldn’t reach the client to update it — is it still installed?";
  return `Client update failed${r.detail ? `: ${r.detail}` : r.error ? ` (${r.error})` : ""}.`;
}

/**
 * Apply whichever updates are pending.
 *
 * The CLIENT goes first and is awaited, by whichever route its install supports — a user-scope
 * `flatpak update`, or the packaged root helper via `punktfunk-client --apply-update`. An
 * install neither can serve is not attempted at all: the user gets the command in a toast,
 * because a button that can only fail teaches nothing.
 *
 * The PLUGIN goes last and is fire-and-forget: Decky's install RPC reinstalls and reloads the
 * plugin, tearing this panel down before any result could arrive. `check` (when passed)
 * refreshes the panel state after a client-only update so the "Update available" button clears.
 */
export async function applyUpdate(
  info: UpdateInfo,
  check?: (force: boolean) => Promise<UpdateInfo | null>,
): Promise<void> {
  if (info.client_update_available && clientUpdateIsOneTap(info)) {
    toaster.toast({
      title: "Punktfunk",
      // A package-manager run is not instant; say so before the wait, not after.
      body:
        info.client_applier === "helper"
          ? "Updating the client — this can take a few minutes…"
          : "Updating the client…",
    });
    try {
      const r = await updateClient();
      toaster.toast({ title: "Punktfunk", body: clientUpdateResultBody(r) });
    } catch {
      toaster.toast({ title: "Punktfunk", body: "Client update failed." });
    }
  } else if (info.client_update_available) {
    // Nothing here can install it — hand over the one line that does, rather than a button
    // that would fail. `client_opt_in` wins when joining the group is what's missing, since
    // that is the step that turns this into a one-tap update from then on.
    const line = info.client_opt_in || info.client_command;
    toaster.toast({
      title: "Punktfunk",
      body: line
        ? `Client update available (${info.client_latest}). Run: ${line}`
        : `A newer client (${info.client_latest}) is available — update it the way you installed it.`,
      duration: 12_000,
    });
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
// Stream launch — via the hidden Steam shortcut (see steam.ts for why it can't be direct).
// ----------------------------------------------------------------------------------------

/**
 * Stream this host. `opts.profileId` streams one of its pinned cards; `opts.requestAccess`
 * runs the supervised launch that waits for the host's operator to approve this Deck.
 *
 * The host is named by REFERENCE (`v.ref`), never by value — no resolution, bitrate or codec
 * ever rides the launch path, which is the same rule the deep-link grammar enforces.
 */
export async function startStream(
  v: HostView,
  opts: LaunchOpts = {},
  label?: string,
): Promise<void> {
  try {
    await launchStream(v.ref, opts);
    Navigation.CloseSideMenus();
    toaster.toast({ title: "Punktfunk", body: `Starting ${label ?? "stream"} — ${v.name}` });
  } catch (e) {
    toaster.toast({ title: "Punktfunk", body: `Launch failed: ${e}` });
  }
}
