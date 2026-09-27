// Launch Punktfunk as Steam games so gamescope focuses + fullscreens them.
//
// THE LAUNCH MECHANISM (verified against MoonDeck): gamescope only gives focus/fullscreen to
// the window tree Steam launched via `reaper` (it detects the "current app" by AppID — see
// gamescope#484). So we cannot launch the flatpak from the plugin backend; we register non-Steam
// shortcuts whose exe is `/bin/sh` running our wrapper script (bin/punktfunkrun.sh), and start
// them with RunGame. The wrapper then execs the flatpak client as a reaper descendant.
//
// TWO shortcuts, both named "Punktfunk" (so they share ONE Steam Input controller-config key —
// see applyControllerConfig):
//   • STREAM  — hidden, stateful: the per-session launcher. Its launch options carry the host
//     reference and the card's preset (PF_REF/PF_PRESET/PF_REQUEST_ACCESS), rewritten per
//     launch, so one shortcut serves every host. Hidden — an implementation detail.
//   • GAMEPAD UI — visible, stateless: fixed launch options = bare `--browse` (PF_BROWSE, no
//     host) → the client's console home (host picker + pairing + settings, gamepad-navigable).
//     This is the library-visible "Punktfunk" app the user opens directly.
//
// Both get the shipped artwork and the native-touch controller config.
//
// A stream started from a Steam game's own page runs under a THIRD kind: a hidden PER-GAME
// shortcut named after the game and wearing the game's own art and icon, so the overlay, the
// "now playing" surfaces and the friends list show the game rather than "Punktfunk". Minted on
// the first Stream tap for that game and reused after; see ensureGameShortcut.

import { applyControllerConfig, gameArt, runnerInfo, saveIcon, shortcutArt } from "./backend";

// SteamClient is a Steam-internal global injected into the CEF context; it is not fully typed
// by @decky/ui, so declare the surface we use. Signatures verified against MoonDeck + the
// decky-frontend-lib SteamClient.Apps typings.
declare const SteamClient: {
  Apps: {
    AddShortcut(
      name: string,
      exePath: string,
      startDir: string,
      launchOptions: string,
    ): Promise<number>;
    SetShortcutName(appId: number, name: string): void;
    SetShortcutExe(appId: number, exe: string): void;
    SetShortcutStartDir(appId: number, dir: string): void;
    SetShortcutIcon(appId: number, iconPath: string): void;
    SetAppLaunchOptions(appId: number, options: string): void;
    // assetType: 0 = grid (portrait capsule), 1 = hero, 2 = logo, 3 = wide grid.
    SetCustomArtworkForApp(
      appId: number,
      base64Image: string,
      imageType: string,
      assetType: number,
    ): Promise<unknown>;
    RunGame(gameId: string, _unused: string, _i: number, _j: number): void;
    TerminateApp(gameId: string, _b: boolean): void;
    RemoveShortcut(appId: number): void;
  };
  GameSessions: {
    // `unAppID` is the 32-bit appid — for a non-Steam shortcut, the one AddShortcut returned.
    RegisterForAppLifetimeNotifications(
      callback: (n: { unAppID: number; nInstanceID: number; bRunning: boolean }) => void,
    ): { unregister(): void };
  };
};

// Steam removed `SteamClient.Apps.SetAppHidden`; visibility goes through
// `collectionStore.SetAppsAsHidden` — but that looks the app up in appStore, which only
// registers a freshly-created shortcut a moment later (calling it immediately throws on a
// null overview). So visibility changes are BEST-EFFORT + DEFERRED, never launch-blocking.
declare const collectionStore:
  | { SetAppsAsHidden?: (appIds: number[], hidden: boolean) => void }
  | undefined;

// SteamUI's appStore indexes every registered app/shortcut by appId; a remembered appId whose
// overview is gone was deleted out from under us (the user removed the library entry). We must
// verify this because the remembered appId lives in Steam's CEF localStorage — which survives a
// plugin UNINSTALL/REINSTALL — so a manually-deleted shortcut otherwise leaves a dangling appId
// that the reuse path below silently repoints (SetShortcut* on a dead id is a no-op), and the
// entry never comes back.
declare const appStore:
  | {
      GetAppOverviewByAppID?: (appId: number) => SteamAppOverviewLike | null;
      allApps?: SteamAppOverviewLike[];
    }
  | undefined;

// The overview surface we read when scanning the library — Steam internals, so everything is
// optional and accessed defensively. Overviews are MobX observables: assigning a field re-sorts
// whatever shelf reads it.
interface SteamAppOverviewLike {
  appid?: number;
  display_name?: string;
  /** Unix seconds; what the Deck's Recent shelf sorts by. */
  rt_last_time_locally_played?: number;
  /** The icon Steam embeds for a shortcut when it loads shortcuts at startup. */
  icon_data?: string;
  icon_data_format?: string;
  BIsShortcut?: () => boolean;
}

// Steam-injected global whose WaitForServicesInitialized resolves once the client's app
// services are up (the MoonDeck-verified readiness signal). Services-init alone doesn't
// guarantee the overview map is populated, so it's paired with the hydration witness below.
declare const App:
  | { WaitForServicesInitialized?: () => Promise<boolean> }
  | undefined;

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

let servicesInitialized: Promise<void> | undefined;
function waitForServicesInitialized(): Promise<void> {
  servicesInitialized ??= (async () => {
    try {
      if (typeof App !== "undefined" && App?.WaitForServicesInitialized) {
        await App.WaitForServicesInitialized();
      }
    } catch {
      /* no signal — the hydration witness still gates the verdict */
    }
  })();
  return servicesInitialized;
}

/** Has appStore demonstrably finished its initial load? An empty `allApps` means "not yet":
 *  any account that ever had our shortcut has at least one app, so a populated map is the
 *  witness that a null overview lookup is an ANSWER rather than a not-loaded-yet. null =
 *  can't tell (missing global, API drift). */
function appStoreHydrated(): boolean | null {
  try {
    if (typeof appStore === "undefined" || !appStore) {
      return null;
    }
    const apps = appStore.allApps;
    return Array.isArray(apps) ? apps.length > 0 : null;
  } catch {
    return null;
  }
}

/** One overview lookup: true = live, false = absent, null = can't tell. */
function queryShortcutAlive(appId: number): boolean | null {
  try {
    // Call it as a METHOD on appStore — NEVER as an extracted function. Its implementation
    // reads the store's own state (`this.m_mapApps`), so `const get = appStore.GetAppOverview…;
    // get(id)` throws on the lost `this`, and the catch below turns that into a permanent
    // "can't tell". `typeof` first: `appStore` is a Steam-injected global, and a bare
    // reference to a missing one is a ReferenceError that optional chaining does NOT prevent.
    if (typeof appStore === "undefined" || !appStore?.GetAppOverviewByAppID) {
      return null;
    }
    return appStore.GetAppOverviewByAppID(appId) != null;
  } catch {
    return null;
  }
}

// How long to wait for the app store before conceding liveness can't be verified. A Deck boot
// hydrates the store within a few seconds of plugin mount; 30 s is comfortably past any real
// boot, and the wait only burns on the absent/unverifiable paths — a live overview answers on
// the first query. Overview registration can trail the bulk hydration by a beat, so a
// "hydrated but absent" verdict gets one grace recheck before it counts as deleted.
const STORE_WAIT_MS = 30_000;
const STORE_POLL_MS = 1_000;
const STORE_GRACE_MS = 2_000;

/** True if a remembered appId still maps to a live Steam shortcut.
 *
 *  The dangerous verdict is FALSE — it sends the caller to AddShortcut, so a wrong "deleted"
 *  mints a duplicate library entry. And a bare null-overview check gets it wrong on EVERY
 *  boot: the plugin mounts while Steam is still starting up, before appStore has registered
 *  its overviews, so the remembered (perfectly live) appId looks up as null and each boot
 *  added another visible "Punktfunk" — the field-reported duplicate pile. Absent is therefore
 *  only believed once the store is demonstrably hydrated; if that can't be established within
 *  budget the answer is true, because a false "alive" merely no-ops Set-calls until the next
 *  ask (and the recreate button re-asks when the store IS ready) while a false "dead"
 *  duplicates forever. */
async function shortcutStillExists(appId: number): Promise<boolean> {
  if (queryShortcutAlive(appId) === true) {
    return true;
  }
  // Race the init signal against the same budget the poll loop gets: a signal that never
  // resolves must not wedge the guard (the single-flight ensure would stay occupied forever).
  await Promise.race([waitForServicesInitialized(), sleep(STORE_WAIT_MS)]);
  for (let waited = 0; waited < STORE_WAIT_MS; waited += STORE_POLL_MS) {
    if (queryShortcutAlive(appId) === true) {
      return true;
    }
    if (appStoreHydrated() === true) {
      await sleep(STORE_GRACE_MS);
      return queryShortcutAlive(appId) !== false; // null = unverifiable → reuse
    }
    await sleep(STORE_POLL_MS);
  }
  return true; // store never became inspectable — reusing beats duplicating
}

/** Set a shortcut's library visibility (best-effort, deferred — the overview registers a moment
 *  after AddShortcut). Hides the stateful stream shortcut; keeps the gamepad-UI one visible. */
function setShortcutHidden(appId: number, hidden: boolean): void {
  const attempt = () => {
    try {
      collectionStore?.SetAppsAsHidden?.([appId], hidden);
    } catch {
      /* overview not registered yet, or the API changed — cosmetic, ignore */
    }
  };
  attempt(); // succeeds immediately for an already-registered (reused) shortcut
  setTimeout(attempt, 2500); // fresh shortcut: retry once its app overview lands
};

// Bump when the shipped artwork changes so existing shortcuts re-apply it once (per appId).
// v3: CI zips through 0.17.1 shipped no assets/ at all, yet v2 was still recorded as applied
// on those installs — the bump makes them re-apply once on the first build that has the files.
const ART_VERSION = 3;
function artKey(appId: number): string {
  return `punktfunk:shortcutArt:${appId}`;
}

/**
 * Apply the plugin's grid/hero/logo/icon to a shortcut (idempotent, once per ART_VERSION per
 * appId). Cosmetic and fully best-effort: any failure is swallowed and retried on the next call.
 */
async function applyArtwork(appId: number, isRetry = false): Promise<void> {
  try {
    if (localStorage.getItem(artKey(appId)) === `${ART_VERSION}`) {
      return;
    }
    const art = await shortcutArt();
    const assets: [string | undefined, number][] = [
      [art.grid, 0],
      [art.hero, 1],
      [art.logo, 2],
      [art.gridwide, 3],
    ];
    let applied = false;
    for (const [data, assetType] of assets) {
      if (data) {
        await SteamClient.Apps.SetCustomArtworkForApp(appId, data, "png", assetType);
        applied = true;
      }
    }
    if (art.icon_path) {
      SteamClient.Apps.SetShortcutIcon(appId, art.icon_path);
      applied = true;
    }
    // Only record "done" when something actually landed — a plugin build whose assets/ is
    // missing/empty must keep retrying on later mounts instead of poisoning the marker.
    if (applied) {
      localStorage.setItem(artKey(appId), `${ART_VERSION}`);
    }
  } catch (e) {
    // A shortcut fresh out of AddShortcut may not be registered yet (the same race
    // setShortcutHidden defers around) — one deferred second attempt, then leave it to
    // the next mount.
    if (!isRetry) {
      setTimeout(() => void applyArtwork(appId, true), 2500);
    }
    console.warn("punktfunk: shortcut artwork not applied", e);
  }
}

// The shortcut name is user-visible (Steam overlay + library) — brand-case it. BOTH shortcuts
// share it so Steam keys them to the SAME controller config (configset key = lowercase name).
const SHORTCUT_NAME = "Punktfunk";

/** Find an existing "Punktfunk" shortcut to ADOPT instead of minting a new library entry — the
 *  healing path for a lost/wiped appId, and for the duplicate piles the boot race left behind
 *  in the field: rebind one of the existing entries to the role rather than adding an N+1th.
 *  (The caller rewrites exe/dir/opts/visibility anyway, so any of them serves.) Only overviews
 *  Steam itself says are shortcuts qualify, and the other role's remembered id is excluded so
 *  the two roles never collapse onto one shortcut. */
function findAdoptableShortcut(excludeAppId: number | null): number | null {
  try {
    if (typeof appStore === "undefined" || !Array.isArray(appStore?.allApps)) {
      return null;
    }
    for (const app of appStore.allApps) {
      if (
        app?.display_name === SHORTCUT_NAME &&
        typeof app.appid === "number" &&
        app.appid !== excludeAppId &&
        app.BIsShortcut?.() === true
      ) {
        return app.appid;
      }
    }
  } catch {
    /* Steam internals drifted — AddShortcut is the fallback */
  }
  return null;
}

/** Remove every "Punktfunk" shortcut beyond the two remembered role ids — the cleanup for
 *  piles already minted by the boot race. Deliberately reachable ONLY from the user-pressed
 *  recreate button, never from mount: automatic library deletion at boot is a bigger hazard
 *  than the mess it would tidy. Returns how many entries were removed. */
function removeDuplicateShortcuts(): number {
  let removed = 0;
  try {
    if (typeof appStore === "undefined" || !Array.isArray(appStore?.allApps)) {
      return 0;
    }
    const keep = [recall(STORAGE_KEY_STREAM), recall(STORAGE_KEY_UI)];
    // Snapshot before removing — RemoveShortcut mutates the store's list under the iteration.
    const surplus = appStore.allApps.filter(
      (app) =>
        app?.display_name === SHORTCUT_NAME &&
        typeof app.appid === "number" &&
        !keep.includes(app.appid) &&
        app.BIsShortcut?.() === true,
    );
    for (const app of surplus) {
      SteamClient.Apps.RemoveShortcut(app.appid as number);
      try {
        localStorage.removeItem(artKey(app.appid as number));
      } catch {
        /* ignore */
      }
      removed++;
    }
  } catch (e) {
    console.warn("punktfunk: duplicate-shortcut sweep incomplete", e);
  }
  return removed;
}

// The shortcut's exe is /bin/sh, NOT the script itself: Decky extracts plugin zips without
// preserving the exec bit, and ~/homebrew/plugins is root-owned so the unprivileged plugin
// backend can't chmod it back on. Passing the script as an argument to the always-executable
// shell removes the +x dependency entirely. SteamOS /bin/sh is bash; the wrapper is plain
// POSIX sh regardless.
const SHELL = "/bin/sh";

// The 64-bit "gameid" RunGame wants, derived from a 32-bit non-Steam shortcut appId: the
// standard non-Steam-game encoding (appid << 32 | 0x02000000). MoonDeck/decky tools use this.
function gameIdFromAppId(appId: number): string {
  return ((BigInt(appId) << 32n) | 0x02000000n).toString();
}

// Persist each shortcut's appId across reloads so we reuse ONE per role instead of churning the
// library (an appId is stable for the life of the shortcut). The STREAM key is the historical
// one, so existing single-shortcut installs migrate into the (now hidden) stream role, and the
// visible gamepad-UI shortcut is created alongside.
const STORAGE_KEY_STREAM = "punktfunk:shortcutAppId";
const STORAGE_KEY_UI = "punktfunk:uiAppId";
// The shortcut the most recent launch ran under — the one Force-stop must end. It may be the
// generic stream shortcut or a per-game one; without this the panel could only stop the former.
const STORAGE_KEY_LAST_LAUNCH = "punktfunk:lastLaunchAppId";
/** `{ref, gameId, title}` of the last game-page stream: what the panel's End game ends. */
const STORAGE_KEY_LAST_GAME = "punktfunk:lastGameStream";

export type GameStreamRef = { ref: string; gameId: string; title: string };

/** The host and title the last game-page stream launched; `null` after a plain stream. */
export function lastGameStream(): GameStreamRef | null {
  try {
    const v = localStorage.getItem(STORAGE_KEY_LAST_GAME);
    return v ? (JSON.parse(v) as GameStreamRef) : null;
  } catch {
    return null;
  }
}

function setLastGameStream(v: GameStreamRef | null): void {
  try {
    if (v) {
      localStorage.setItem(STORAGE_KEY_LAST_GAME, JSON.stringify(v));
    } else {
      localStorage.removeItem(STORAGE_KEY_LAST_GAME);
    }
  } catch {
    /* ignore */
  }
}
// `punktfunk:gameShortcut:<steam appid>` → the per-game shortcut's appId.
const GAME_KEY_PREFIX = "punktfunk:gameShortcut:";

function remember(key: string, appId: number) {
  try {
    localStorage.setItem(key, String(appId));
  } catch {
    /* ignore */
  }
}
function recall(key: string): number | null {
  try {
    const v = localStorage.getItem(key);
    return v ? Number(v) : null;
  } catch {
    return null;
  }
}

// Install the native-touch controller config once per plugin session (idempotent file writes in
// the root backend). Keyed by the shared shortcut NAME, so this single call covers both
// shortcuts. Gated in localStorage so we don't rewrite Steam's config dir on every launch; bump
// CONFIG_VERSION to force a reinstall after the shipped .vdf changes.
const CONFIG_KEY = "punktfunk:controllerConfig";
// Per-game layouts hang off the same name, one per game title (see ensureGameControllerConfig).
const GAME_CONFIG_PREFIX = "punktfunk:controllerConfig:";
const CONFIG_VERSION = 1;
async function ensureControllerConfig(): Promise<void> {
  try {
    if (localStorage.getItem(CONFIG_KEY) === `${CONFIG_VERSION}`) {
      return;
    }
    const r = await applyControllerConfig(SHORTCUT_NAME);
    // `ok` alone isn't done: with zero account configset dirs (fresh Steam) the backend
    // succeeds without pointing any account at the template — keep retrying until one lands.
    if (r?.ok && (r.applied ?? []).some((a) => a.startsWith("configset:"))) {
      localStorage.setItem(CONFIG_KEY, `${CONFIG_VERSION}`);
    } else {
      console.warn("punktfunk: controller config not fully applied", r);
    }
  } catch (e) {
    console.warn("punktfunk: controller config not applied", e);
  }
}

/**
 * Ensure the STREAM shortcut (hidden, stateful) — the per-session launcher whose launch options
 * are rewritten per stream. Branded, artworked, native-touch config applied, and HIDDEN (it is
 * an implementation detail; the visible entry is the gamepad-UI shortcut). Returns its appId +
 * the current runner path. Reuses/repoints the remembered shortcut (the plugin dir can change
 * across reinstalls, and pre-two-shortcut installs had this one visible).
 */
async function doEnsureStreamShortcut(): Promise<{ appId: number; runner: string; clientBin: string }> {
  const info = await runnerInfo();
  if (!info.exists) {
    throw new Error(`launch wrapper missing at ${info.runner}`);
  }
  const startDir = info.runner.replace(/\/[^/]*$/, ""); // the plugin's bin/ dir
  void ensureControllerConfig(); // fire-and-forget — never blocks the launch

  // Reuse the remembered shortcut only if it still exists — a stale appId (shortcut deleted, key
  // outlived it across a reinstall) must fall through, not be silently repointed. On a lost id,
  // ADOPT an existing same-named shortcut before AddShortcut so a wiped key never duplicates.
  const remembered = recall(STORAGE_KEY_STREAM);
  let appId =
    remembered != null && (await shortcutStillExists(remembered)) ? remembered : null;
  if (appId == null) {
    appId =
      findAdoptableShortcut(recall(STORAGE_KEY_UI)) ??
      (await SteamClient.Apps.AddShortcut(SHORTCUT_NAME, SHELL, startDir, ""));
    remember(STORAGE_KEY_STREAM, appId);
  }
  SteamClient.Apps.SetShortcutExe(appId, SHELL);
  SteamClient.Apps.SetShortcutStartDir(appId, startDir);
  SteamClient.Apps.SetShortcutName(appId, SHORTCUT_NAME);
  setShortcutHidden(appId, true); // also migrates pre-two-shortcut installs (were visible)
  void applyArtwork(appId);
  return { appId, runner: info.runner, clientBin: info.client_bin ?? "" };
}

// Concurrent ensure calls share one run per role — two ensures racing past the liveness check
// would each AddShortcut, which is exactly the duplicate class this file exists to prevent (and
// the store-readiness wait makes the window real: mount's fire-and-forget ensure can be mid-wait
// when a QAM press arrives). Sequential calls still re-run, so per-launch repointing is kept.
let streamEnsureInFlight: Promise<{ appId: number; runner: string; clientBin: string }> | null =
  null;
function ensureStreamShortcut(): Promise<{ appId: number; runner: string; clientBin: string }> {
  streamEnsureInFlight ??= doEnsureStreamShortcut().finally(() => {
    streamEnsureInFlight = null;
  });
  return streamEnsureInFlight;
}

/**
 * Ensure the GAMEPAD-UI shortcut (visible, stateless) — the library-facing "Punktfunk" entry
 * that opens the client's console home (bare `--browse`: host picker + pairing + settings).
 * Fixed launch options (no per-session state), branded, artworked, native-touch config applied,
 * kept VISIBLE. Idempotent — call on plugin mount so the library entry always exists and stays
 * repointed to the current plugin dir. Best-effort: returns null on any failure.
 */
async function doEnsureGamepadUiShortcut(): Promise<number | null> {
  try {
    const info = await runnerInfo();
    if (!info.exists) {
      return null;
    }
    const startDir = info.runner.replace(/\/[^/]*$/, "");
    void ensureControllerConfig();
    // PF_BROWSE → the wrapper runs the SESSION's `--browse --fullscreen` (console home), which is
    // the one branch this rework deliberately left alone. %command% expands to the shortcut exe
    // (/bin/sh); the wrapper rides behind as an arg. PF_CLIENT_BIN only when the backend resolved
    // a NATIVE client — else the wrapper's flatpak default stands and this shortcut is exactly
    // what it always was.
    const clientBin = safeClientBin(info.client_bin) ? `PF_CLIENT_BIN=${info.client_bin} ` : "";
    const launchOpts = `${clientBin}PF_BROWSE=1 %command% "${info.runner}"`;

    // Reuse the remembered entry only if it still exists; a stale appId (deleted shortcut whose
    // localStorage key survived a plugin reinstall) falls through so the visible library entry
    // actually comes back instead of repointing a dead id. On a lost id, ADOPT an existing
    // same-named shortcut (a boot-race duplicate, or the entry whose key was wiped) before
    // AddShortcut — creation is the last resort, never the response to a mere lookup miss.
    let appId = recall(STORAGE_KEY_UI);
    if (appId == null || !(await shortcutStillExists(appId))) {
      appId =
        findAdoptableShortcut(recall(STORAGE_KEY_STREAM)) ??
        (await SteamClient.Apps.AddShortcut(SHORTCUT_NAME, SHELL, startDir, ""));
      remember(STORAGE_KEY_UI, appId);
    }
    SteamClient.Apps.SetShortcutExe(appId, SHELL);
    SteamClient.Apps.SetShortcutStartDir(appId, startDir);
    SteamClient.Apps.SetShortcutName(appId, SHORTCUT_NAME);
    SteamClient.Apps.SetAppLaunchOptions(appId, launchOpts);
    setShortcutHidden(appId, false); // the visible library entry
    void applyArtwork(appId);
    return appId;
  } catch (e) {
    console.warn("punktfunk: gamepad-UI shortcut not ensured", e);
    return null;
  }
}

// Same single-flight rule as the stream role (see ensureStreamShortcut).
let uiEnsureInFlight: Promise<number | null> | null = null;
export function ensureGamepadUiShortcut(): Promise<number | null> {
  uiEnsureInFlight ??= doEnsureGamepadUiShortcut().finally(() => {
    uiEnsureInFlight = null;
  });
  return uiEnsureInFlight;
}

/**
 * Force the visible "Punktfunk" library entry back into existence — the recovery button for
 * "my shortcut disappeared". Drops any remembered appId that no longer maps to a live shortcut
 * (so it can't shadow a fresh AddShortcut), then re-ensures. Safe to press anytime: a shortcut
 * that still exists is left in place (no duplicate); a missing one is recreated. Covers the case
 * self-heal-on-mount can't — deleting the shortcut WITHOUT reinstalling (no mount → no ensure).
 * Also sweeps surplus "Punktfunk" entries (the piles the boot race minted before the store-
 * readiness gate existed) — the button is where that cleanup lives, never mount. Returns the
 * (new or existing) visible appId (null on failure) plus how many duplicates were removed.
 */
export async function recreateShortcuts(): Promise<{
  appId: number | null;
  removedDuplicates: number;
}> {
  for (const key of [STORAGE_KEY_STREAM, STORAGE_KEY_UI]) {
    const id = recall(key);
    if (id != null && !(await shortcutStillExists(id))) {
      try {
        localStorage.removeItem(artKey(id)); // stale art marker for the dead appId
        localStorage.removeItem(key);
      } catch {
        /* ignore */
      }
    }
  }
  // Recreate the visible entry now; the hidden stream shortcut re-registers lazily on next
  // launch. Sweep AFTER the ensure so the remembered ids are fresh — and only when the ensure
  // succeeded: on a failed ensure the "keep" list can't be trusted, and deleting candidates a
  // later ensure would adopt could leave the library with no entry at all.
  const appId = await ensureGamepadUiShortcut();
  const removedDuplicates = appId != null ? removeDuplicateShortcuts() : 0;
  return { appId, removedDuplicates };
}

// ----------------------------------------------------------------------------------------
// Per-game shortcuts — the stream that looks like the game.
// ----------------------------------------------------------------------------------------

function gameKey(steamAppId: number): string {
  return `${GAME_KEY_PREFIX}${steamAppId}`;
}

/** The per-game shortcut minted for this Steam title, if any (liveness not checked here). */
export function gameShortcutFor(steamAppId: number): number | null {
  return recall(gameKey(steamAppId));
}

// Read from storage once and kept in step with our own writes — nothing else touches these
// keys. isOurShortcut runs for every app Steam starts or stops, and a full storage scan there
// is work for nothing.
let pairCache: Array<[number, number]> | null = null;

/** Every (steam appid → shortcut appId) pair on record. */
function gameShortcutPairs(): Array<[number, number]> {
  if (pairCache) {
    return pairCache;
  }
  const pairs: Array<[number, number]> = [];
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (!key?.startsWith(GAME_KEY_PREFIX)) {
        continue;
      }
      const steamAppId = Number(key.slice(GAME_KEY_PREFIX.length));
      const shortcut = Number(localStorage.getItem(key));
      if (Number.isFinite(steamAppId) && Number.isFinite(shortcut) && shortcut > 0) {
        pairs.push([steamAppId, shortcut]);
      }
    }
    pairCache = pairs; // only a scan that finished; a failed one must be retried, not cached
  } catch {
    /* storage unavailable */
  }
  return pairs;
}

/** Record the shortcut minted for a title; the pair cache is rebuilt on the next read. */
function rememberGameShortcut(steamAppId: number, appId: number): void {
  remember(gameKey(steamAppId), appId);
  pairCache = null;
}

/** Every key under `prefix`, gone. Collected before removing — removing shifts the indices. */
function removeByPrefix(prefix: string): void {
  try {
    const keys: string[] = [];
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (key?.startsWith(prefix)) {
        keys.push(key);
      }
    }
    for (const key of keys) {
      localStorage.removeItem(key);
    }
  } catch {
    /* storage unavailable */
  }
}

/** The Steam title a per-game shortcut stands for, or null for any other appid. */
export function steamAppIdForShortcut(shortcutAppId: number): number | null {
  return gameShortcutPairs().find(([, s]) => s === shortcutAppId)?.[0] ?? null;
}

// Bump when what applyGameArtwork fetches changes, so existing per-game shortcuts re-apply.
// v3: the icon is written as PNG and injected into the live overview.
const GAME_ART_VERSION = 3;

/** Steam's cached icons are JPG; a shortcut icon has to be PNG (or ICO). Decode on a canvas. */
async function toPng(base64: string, type: string): Promise<string> {
  if (type === "png") {
    return base64;
  }
  const img = new Image();
  img.src = `data:image/${type === "jpg" ? "jpeg" : type};base64,${base64}`;
  await img.decode();
  const canvas = document.createElement("canvas");
  canvas.width = img.naturalWidth || 32;
  canvas.height = img.naturalHeight || 32;
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    throw new Error("no 2d canvas");
  }
  ctx.drawImage(img, 0, 0);
  return canvas.toDataURL("image/png").split(",")[1];
}

/**
 * The icon for a per-game shortcut, two ways: the PNG file for Steam's next start (it embeds a
 * shortcut's icon into the overview only when loading shortcuts), and the bytes into the live
 * overview now, which is what the Steam menu draws this session.
 */
async function applyGameIcon(
  shortcutAppId: number,
  steamAppId: number,
  art: { icon?: string; icon_type?: string; icon_path?: string },
): Promise<boolean> {
  if (!art.icon) {
    return false;
  }
  const png = await toPng(art.icon, art.icon_type ?? "jpg");
  let path = art.icon_path ?? "";
  if (!path) {
    const saved = await saveIcon(steamAppId, png);
    path = saved.ok && saved.path ? saved.path : "";
  }
  if (path) {
    SteamClient.Apps.SetShortcutIcon(shortcutAppId, path);
  }
  try {
    const overview = appStore?.GetAppOverviewByAppID?.(shortcutAppId);
    if (overview) {
      overview.icon_data = png;
      overview.icon_data_format = "png";
    }
  } catch {
    /* the file still lands for the next start */
  }
  return !!path;
}
function gameArtKey(shortcutAppId: number): string {
  return `punktfunk:gameArt:${shortcutAppId}`;
}

/** Dress a per-game shortcut in the game's own grid/hero/logo/header and icon. Once per version;
 *  cosmetic and best-effort, with one deferred retry for a shortcut not yet registered. */
async function applyGameArtwork(
  shortcutAppId: number,
  steamAppId: number,
  iconHash: string,
  isRetry = false,
): Promise<void> {
  try {
    if (localStorage.getItem(gameArtKey(shortcutAppId)) === `${GAME_ART_VERSION}`) {
      return;
    }
    const art = await gameArt(steamAppId, iconHash);
    if (!art.ok) {
      return;
    }
    const assets: Array<[string | undefined, string | undefined, number]> = [
      [art.grid, art.grid_type, 0],
      [art.hero, art.hero_type, 1],
      [art.logo, art.logo_type, 2],
      [art.gridwide, art.gridwide_type, 3],
    ];
    let applied = false;
    for (const [data, type, assetType] of assets) {
      if (data) {
        await SteamClient.Apps.SetCustomArtworkForApp(shortcutAppId, data, type ?? "jpg", assetType);
        applied = true;
      }
    }
    const iconLanded = await applyGameIcon(shortcutAppId, steamAppId, art);
    // Done only when the icon landed too: it is the one piece the overlay shows on every
    // frame, and a gray box there must be retried on the next launch, not recorded as fine.
    if (applied && iconLanded) {
      localStorage.setItem(gameArtKey(shortcutAppId), `${GAME_ART_VERSION}`);
    }
  } catch (e) {
    if (!isRetry) {
      setTimeout(() => void applyGameArtwork(shortcutAppId, steamAppId, iconHash, true), 2500);
    }
    console.warn("punktfunk: game artwork not applied", e);
  }
}

/** Point a per-game shortcut at the native-touch layout. Steam keys the configset by the
 *  shortcut's lowercase NAME, so each game name needs its own entry; once per name. */
async function ensureGameControllerConfig(title: string): Promise<void> {
  // The name is written into a VDF as a quoted key; one carrying a quote would corrupt the
  // file. Such a game keeps Steam's default layout — a cosmetic loss, not a broken file.
  if (title.includes('"')) {
    return;
  }
  const key = `${GAME_CONFIG_PREFIX}${title.toLowerCase()}`;
  try {
    if (localStorage.getItem(key) === `${CONFIG_VERSION}`) {
      return;
    }
    const r = await applyControllerConfig(title);
    if (r?.ok && (r.applied ?? []).some((a) => a.startsWith("configset:"))) {
      localStorage.setItem(key, `${CONFIG_VERSION}`);
    }
  } catch (e) {
    console.warn("punktfunk: game controller config not applied", e);
  }
}

/**
 * Ensure the hidden per-game shortcut for a Steam title: named after the game, dressed in its
 * art, pointed at the touch layout. Reused across launches by appid; recreated if the user
 * removed it. Same liveness rule as the generic roles — a wrong "deleted" mints a duplicate.
 */
async function doEnsureGameShortcut(
  steamAppId: number,
  title: string,
  iconHash: string,
): Promise<{ appId: number; runner: string; clientBin: string }> {
  const info = await runnerInfo();
  if (!info.exists) {
    throw new Error(`launch wrapper missing at ${info.runner}`);
  }
  const startDir = info.runner.replace(/\/[^/]*$/, "");
  const remembered = gameShortcutFor(steamAppId);
  let appId =
    remembered != null && (await shortcutStillExists(remembered)) ? remembered : null;
  if (appId == null) {
    appId = await SteamClient.Apps.AddShortcut(title, SHELL, startDir, "");
    rememberGameShortcut(steamAppId, appId);
    try {
      localStorage.removeItem(gameArtKey(appId)); // a recycled appId must not skip its art
    } catch {
      /* ignore */
    }
  }
  SteamClient.Apps.SetShortcutExe(appId, SHELL);
  SteamClient.Apps.SetShortcutStartDir(appId, startDir);
  SteamClient.Apps.SetShortcutName(appId, title);
  setShortcutHidden(appId, true);
  void applyGameArtwork(appId, steamAppId, iconHash);
  void ensureGameControllerConfig(title);
  return { appId, runner: info.runner, clientBin: info.client_bin ?? "" };
}

const gameEnsureInFlight = new Map<number, Promise<{ appId: number; runner: string; clientBin: string }>>();
function ensureGameShortcut(
  steamAppId: number,
  title: string,
  iconHash: string,
): Promise<{ appId: number; runner: string; clientBin: string }> {
  let p = gameEnsureInFlight.get(steamAppId);
  if (!p) {
    p = doEnsureGameShortcut(steamAppId, title, iconHash).finally(() => {
      gameEnsureInFlight.delete(steamAppId);
    });
    gameEnsureInFlight.set(steamAppId, p);
  }
  return p;
}

/** Remove every per-game shortcut on record — the cleanup button. Returns how many went. */
export function removeGameShortcuts(): number {
  let removed = 0;
  for (const [steamAppId, shortcut] of gameShortcutPairs()) {
    try {
      if (queryShortcutAlive(shortcut) !== false) {
        SteamClient.Apps.RemoveShortcut(shortcut);
        removed++;
      }
      localStorage.removeItem(gameKey(steamAppId));
      localStorage.removeItem(gameArtKey(shortcut));
      running.delete(shortcut); // ours only — a generic stream that is up keeps its state
    } catch (e) {
      console.warn("punktfunk: game shortcut not removed", e);
    }
  }
  // The per-game layouts are keyed by game name, so no appid can find them.
  removeByPrefix(GAME_CONFIG_PREFIX);
  pairCache = null;
  notifyRunning();
  return removed;
}

// ----------------------------------------------------------------------------------------
// Running state — which of our shortcuts Steam says is up, so the Play button can read Stop.
// ----------------------------------------------------------------------------------------

const running = new Set<number>(); // shortcut appIds
const runningListeners = new Set<() => void>();

function notifyRunning(): void {
  for (const listener of runningListeners) {
    listener();
  }
}

export function subscribeRunning(listener: () => void): () => void {
  runningListeners.add(listener);
  return () => {
    runningListeners.delete(listener);
  };
}

function isOurShortcut(appId: number): boolean {
  return appId === recall(STORAGE_KEY_STREAM) || steamAppIdForShortcut(appId) != null;
}

/**
 * Copy the per-game shortcut's last-played time onto the Steam title, so the GAME climbs the
 * Deck's Recent shelf after a stream — a hidden entry never shows there. Steam persists the
 * shortcut's own timestamp, so re-applying at load carries it across reboots. Only ever moves
 * forward; a title played locally more recently keeps its own time.
 */
function mirrorLastPlayed(steamAppId: number, shortcutAppId: number): void {
  try {
    if (typeof appStore === "undefined" || !appStore?.GetAppOverviewByAppID) {
      return;
    }
    const from = appStore.GetAppOverviewByAppID(shortcutAppId);
    const to = appStore.GetAppOverviewByAppID(steamAppId);
    const played = from?.rt_last_time_locally_played;
    if (!to || typeof played !== "number" || played <= (to.rt_last_time_locally_played ?? 0)) {
      return;
    }
    to.rt_last_time_locally_played = played;
  } catch (e) {
    console.warn("punktfunk: last-played not mirrored", e);
  }
}

function mirrorAllLastPlayed(): void {
  for (const [steamAppId, shortcut] of gameShortcutPairs()) {
    mirrorLastPlayed(steamAppId, shortcut);
  }
}

/** Follow Steam's app lifetime feed for our shortcuts. Returns the unregister for dismount. */
export function watchRunningStreams(): () => void {
  // Re-apply the mirrored timestamps once the store is readable — a fresh boot's Recent shelf
  // should already show last night's stream.
  void (async () => {
    await Promise.race([waitForServicesInitialized(), sleep(STORE_WAIT_MS)]);
    await sleep(STORE_GRACE_MS);
    mirrorAllLastPlayed();
  })();
  try {
    const reg = SteamClient.GameSessions.RegisterForAppLifetimeNotifications((n) => {
      if (!isOurShortcut(n.unAppID)) {
        return;
      }
      if (n.bRunning) {
        running.add(n.unAppID);
      } else {
        running.delete(n.unAppID);
      }
      notifyRunning();
      const steamAppId = steamAppIdForShortcut(n.unAppID);
      if (steamAppId != null) {
        // Steam stamps the shortcut at start; a beat later it is readable here.
        setTimeout(() => mirrorLastPlayed(steamAppId, n.unAppID), 1500);
      }
    });
    return () => reg.unregister();
  } catch (e) {
    console.warn("punktfunk: app lifetime feed unavailable", e);
    return () => {};
  }
}

/** Is this Steam title being streamed right now (its per-game shortcut is up)? */
export function isGameStreaming(steamAppId: number): boolean {
  const shortcut = gameShortcutFor(steamAppId);
  return shortcut != null && running.has(shortcut);
}

/** End the stream of this Steam title — Steam's Stop, for our button. */
export function stopGameStream(steamAppId: number): void {
  const shortcut = gameShortcutFor(steamAppId);
  if (shortcut != null) {
    SteamClient.Apps.TerminateApp(gameIdFromAppId(shortcut), false);
  }
}

/** Launch the stateless gamepad-UI shortcut (console home) from the plugin, e.g. a QAM button. */
export async function launchGamepadUi(): Promise<void> {
  const appId = await ensureGamepadUiShortcut();
  if (appId != null) {
    SteamClient.Apps.RunGame(gameIdFromAppId(appId), "", -1, 100);
  }
}

/** Per-launch extras beyond the host reference (all optional — {} is the plain stream). */
export interface LaunchOpts {
  /** A pinned card: stream with this preset, one-off (PF_PRESET → `--profile`). */
  presetId?: string;
  /**
   * A title in the host's library to launch into the stream (PF_GAME → `--game`), by its
   * store-qualified id (`steam:570`). The host resolves it; the Deck only names it.
   */
  gameId?: string;
  /**
   * Ask the host's operator to admit this Deck rather than typing a PIN (PF_REQUEST_ACCESS).
   * The connect PARKS until somebody approves it, and the launch runs SUPERVISED — see the
   * wrapper for why `--exec` is dropped on this path alone.
   */
  requestAccess?: boolean;
}

// Host refs and preset ids ride Steam launch options as env-prefix tokens (`PF_REF=<ref>`),
// so they must be space/quote-free — Steam's tokenizer and the wrapper's env both break
// otherwise. Real values are UUIDs or `addr:port`, so this rejects nothing in practice; it is
// VALIDATION, never encoding (the client must receive the opaque token verbatim).
const UNSAFE_TOKEN = /["'\\$`\s]/;
export function isSafeLaunchId(id: string): boolean {
  return (
    id.length > 0 &&
    id.length <= 128 &&
    UNSAFE_TOKEN.exec(id) === null &&
    /^[\x21-\x7e]+$/.test(id)
  );
}

/**
 * Is a resolved native-client path safe to put in Steam's launch options? Same rule, separate
 * name because the failure is different: an unsafe id is a bug in our own data, an unsafe path
 * is just where the user installed the client — so the browse shortcut degrades to its flatpak
 * default rather than refusing to exist.
 */
function safeClientBin(bin: string | undefined): bin is string {
  return !!bin && isSafeLaunchId(bin);
}

/**
 * Stream `ref` fullscreen in Gaming Mode, optionally with a pinned card's preset. Encodes the
 * target into the STREAM shortcut's launch options — one hidden shortcut serves every host —
 * then RunGame.
 *
 * No Wake-on-LAN here any more. The plugin used to fire a magic packet itself and then stretch
 * the connect budget to 75 s to cover the host's resume, which was a workaround for the era
 * before the CLI existed. `punktfunk launch` now runs the real wake-and-wait loop (packet at
 * t=0, re-sent every 6 s, presence polled every second) and only dials once the host answers —
 * strictly better, and it deletes a backend method, a frontend call and a shell branch.
 */
export async function launchStream(ref: string, opts: LaunchOpts = {}): Promise<void> {
  validateLaunch(ref, opts);
  const { appId, runner, clientBin } = await ensureStreamShortcut();
  setLastGameStream(opts.gameId ? { ref, gameId: opts.gameId, title: opts.gameId } : null);
  runShortcut(appId, launchOptions(ref, runner, clientBin, opts));
}

/**
 * Stream a Steam title from its own page: the same launch as `launchStream` with the game
 * named, but under the hidden per-game shortcut so Steam shows the game running, not Punktfunk.
 * `title` and `iconHash` come from Steam's overview of the game and only dress the shortcut.
 */
export async function launchGameStream(
  ref: string,
  steamAppId: number,
  title: string,
  iconHash: string,
  opts: LaunchOpts = {},
): Promise<void> {
  const full = { ...opts, gameId: `steam:${steamAppId}` };
  validateLaunch(ref, full);
  const { appId, runner, clientBin } = await ensureGameShortcut(steamAppId, title, iconHash);
  setLastGameStream({ ref, gameId: full.gameId, title });
  runShortcut(appId, launchOptions(ref, runner, clientBin, full));
}

function validateLaunch(ref: string, opts: LaunchOpts): void {
  if (!isSafeLaunchId(ref)) {
    throw new Error(`unsupported host reference: ${ref}`);
  }
  if (opts.presetId && !isSafeLaunchId(opts.presetId)) {
    throw new Error(`unsupported preset id: ${opts.presetId}`);
  }
  if (opts.gameId && !isSafeLaunchId(opts.gameId)) {
    throw new Error(`unsupported game id: ${opts.gameId}`);
  }
}

function runShortcut(appId: number, options: string): void {
  SteamClient.Apps.SetAppLaunchOptions(appId, options);
  remember(STORAGE_KEY_LAST_LAUNCH, appId);
  SteamClient.Apps.RunGame(gameIdFromAppId(appId), "", -1, 100);
}

/** The Steam launch options for one stream: `KEY=value … %command% "<runner>"`. */
function launchOptions(ref: string, runner: string, clientBin: string, opts: LaunchOpts): string {
  const env = [`PF_REF=${ref}`];
  // Set only for a NATIVE client install; absent, the wrapper takes its flatpak default, so every
  // existing Deck install produces byte-identical launch options to before.
  if (clientBin) {
    // The one launch-option value that comes from the backend rather than a store id, and so
    // the one that could carry a space: a path like `/home/deck/my apps/punktfunk-client` would
    // split Steam's tokenizer and land its tail in front of %command% as a bogus env token.
    if (!isSafeLaunchId(clientBin)) {
      throw new Error(`client path can't ride Steam's launch options: ${clientBin}`);
    }
    env.push(`PF_CLIENT_BIN=${clientBin}`);
  }
  if (opts.presetId) {
    env.push(`PF_PRESET=${opts.presetId}`);
  }
  if (opts.gameId) {
    env.push(`PF_GAME=${opts.gameId}`);
  }
  if (opts.requestAccess) {
    env.push("PF_REQUEST_ACCESS=1");
  }
  // KEY=value ... %command% args — %command% expands to the shortcut exe (/bin/sh); the wrapper
  // script rides behind it as an argument and reads PF_* from the environment.
  return `${env.join(" ")} %command% "${runner}"`;
}

/** Stop the running stream — whichever shortcut the last launch ran under (best-effort; the
 *  in-stream chord/back also works). */
export function stopStream(): void {
  const appId = recall(STORAGE_KEY_LAST_LAUNCH) ?? recall(STORAGE_KEY_STREAM);
  if (appId != null) {
    SteamClient.Apps.TerminateApp(gameIdFromAppId(appId), false);
  }
}
