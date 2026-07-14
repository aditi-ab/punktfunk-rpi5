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
//   • STREAM  — hidden, stateful: the per-session launcher. Its launch options carry the host /
//     pinned game (PF_HOST/PF_LAUNCH/PF_BROWSE), rewritten per launch, so one shortcut serves
//     every host. Driven by the QAM/pins/host-library actions. Hidden — an implementation detail.
//   • GAMEPAD UI — visible, stateless: fixed launch options = bare `--browse` (PF_BROWSE, no
//     host) → the client's console home (host picker + pairing + settings, gamepad-navigable).
//     This is the library-visible "Punktfunk" app the user opens directly.
//
// Both get the shipped artwork and the native-touch controller config.

import { applyControllerConfig, runnerInfo, shortcutArt, wake } from "./backend";

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
  };
};

// Steam removed `SteamClient.Apps.SetAppHidden`; visibility goes through
// `collectionStore.SetAppsAsHidden` — but that looks the app up in appStore, which only
// registers a freshly-created shortcut a moment later (calling it immediately throws on a
// null overview). So visibility changes are BEST-EFFORT + DEFERRED, never launch-blocking.
declare const collectionStore:
  | { SetAppsAsHidden?: (appIds: number[], hidden: boolean) => void }
  | undefined;

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
const ART_VERSION = 2;
function artKey(appId: number): string {
  return `punktfunk:shortcutArt:${appId}`;
}

/**
 * Apply the plugin's grid/hero/logo/icon to a shortcut (idempotent, once per ART_VERSION per
 * appId). Cosmetic and fully best-effort: any failure is swallowed and retried on the next call.
 */
async function applyArtwork(appId: number): Promise<void> {
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
    for (const [data, assetType] of assets) {
      if (data) {
        await SteamClient.Apps.SetCustomArtworkForApp(appId, data, "png", assetType);
      }
    }
    if (art.icon_path) {
      SteamClient.Apps.SetShortcutIcon(appId, art.icon_path);
    }
    localStorage.setItem(artKey(appId), `${ART_VERSION}`);
  } catch (e) {
    console.warn("punktfunk: shortcut artwork not applied", e);
  }
}

// The shortcut name is user-visible (Steam overlay + library) — brand-case it. BOTH shortcuts
// share it so Steam keys them to the SAME controller config (configset key = lowercase name).
const SHORTCUT_NAME = "Punktfunk";

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
const CONFIG_VERSION = 1;
async function ensureControllerConfig(): Promise<void> {
  try {
    if (localStorage.getItem(CONFIG_KEY) === `${CONFIG_VERSION}`) {
      return;
    }
    const r = await applyControllerConfig(SHORTCUT_NAME);
    if (r?.ok) {
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
async function ensureStreamShortcut(): Promise<{ appId: number; runner: string }> {
  const info = await runnerInfo();
  if (!info.exists) {
    throw new Error(`launch wrapper missing at ${info.runner}`);
  }
  const startDir = info.runner.replace(/\/[^/]*$/, ""); // the plugin's bin/ dir
  void ensureControllerConfig(); // fire-and-forget — never blocks the launch

  const remembered = recall(STORAGE_KEY_STREAM);
  if (remembered != null) {
    SteamClient.Apps.SetShortcutExe(remembered, SHELL);
    SteamClient.Apps.SetShortcutStartDir(remembered, startDir);
    SteamClient.Apps.SetShortcutName(remembered, SHORTCUT_NAME);
    setShortcutHidden(remembered, true); // migrate pre-two-shortcut installs (were visible)
    void applyArtwork(remembered);
    return { appId: remembered, runner: info.runner };
  }

  const appId = await SteamClient.Apps.AddShortcut(SHORTCUT_NAME, SHELL, startDir, "");
  SteamClient.Apps.SetShortcutName(appId, SHORTCUT_NAME);
  setShortcutHidden(appId, true);
  void applyArtwork(appId);
  remember(STORAGE_KEY_STREAM, appId);
  return { appId, runner: info.runner };
}

/**
 * Ensure the GAMEPAD-UI shortcut (visible, stateless) — the library-facing "Punktfunk" entry
 * that opens the client's console home (bare `--browse`: host picker + pairing + settings).
 * Fixed launch options (no per-session state), branded, artworked, native-touch config applied,
 * kept VISIBLE. Idempotent — call on plugin mount so the library entry always exists and stays
 * repointed to the current plugin dir. Best-effort: returns null on any failure.
 */
export async function ensureGamepadUiShortcut(): Promise<number | null> {
  try {
    const info = await runnerInfo();
    if (!info.exists) {
      return null;
    }
    const startDir = info.runner.replace(/\/[^/]*$/, "");
    void ensureControllerConfig();
    // Bare browse: PF_BROWSE with no PF_HOST → the wrapper runs `--browse --fullscreen` (console
    // home). %command% expands to the shortcut exe (/bin/sh); the wrapper rides behind as an arg.
    const launchOpts = `PF_BROWSE=1 %command% "${info.runner}"`;

    let appId = recall(STORAGE_KEY_UI);
    if (appId != null) {
      SteamClient.Apps.SetShortcutExe(appId, SHELL);
      SteamClient.Apps.SetShortcutStartDir(appId, startDir);
      SteamClient.Apps.SetShortcutName(appId, SHORTCUT_NAME);
    } else {
      appId = await SteamClient.Apps.AddShortcut(SHORTCUT_NAME, SHELL, startDir, "");
      SteamClient.Apps.SetShortcutName(appId, SHORTCUT_NAME);
      remember(STORAGE_KEY_UI, appId);
    }
    SteamClient.Apps.SetAppLaunchOptions(appId, launchOpts);
    setShortcutHidden(appId, false); // the visible library entry
    void applyArtwork(appId);
    return appId;
  } catch (e) {
    console.warn("punktfunk: gamepad-UI shortcut not ensured", e);
    return null;
  }
}

/** Launch the stateless gamepad-UI shortcut (console home) from the plugin, e.g. a QAM button. */
export async function launchGamepadUi(): Promise<void> {
  const appId = await ensureGamepadUiShortcut();
  if (appId != null) {
    SteamClient.Apps.RunGame(gameIdFromAppId(appId), "", -1, 100);
  }
}

/** Per-launch extras beyond the host target (all optional — {} is the plain stream). */
export interface LaunchOpts {
  /** Library id to launch on connect (a pinned game) — rides PF_LAUNCH → `--launch`. */
  launchId?: string;
  /** Open the gamepad library launcher instead of streaming (PF_BROWSE → `--browse`). */
  browse?: boolean;
  /** Management-API port for the launcher's library fetch (PF_MGMT; 0/absent = default). */
  mgmt?: number;
}

// Launch ids ride Steam launch options as an env-prefix token (`PF_LAUNCH=<id>`), so they
// must be space/quote-free — Steam's tokenizer and the wrapper's env both break otherwise.
// Real ids are `steam:<digits>` / `custom:<slug>`, so this rejects nothing in practice;
// it's VALIDATION, never encoding (the host must match the opaque token verbatim).
const UNSAFE_LAUNCH_ID = /["'\\$`\s]/;
export function isSafeLaunchId(id: string): boolean {
  return (
    id.length > 0 &&
    id.length <= 128 &&
    UNSAFE_LAUNCH_ID.exec(id) === null &&
    /^[\x21-\x7e]+$/.test(id)
  );
}

/**
 * Launch a stream to `host:port` fullscreen in Gaming Mode (optionally straight into a
 * library title, or into a host's gamepad library). Encodes the target into the STREAM
 * shortcut's launch options (so one hidden shortcut serves every host and every pinned game),
 * then RunGame.
 */
export async function launchStream(
  host: string,
  port: number,
  opts: LaunchOpts = {},
): Promise<void> {
  // Wake-on-LAN: if this host is asleep, nudge it awake before the stream connects. Kicked off now
  // so it races with the shortcut setup (near-zero added latency), and awaited just before RunGame.
  // Best-effort — the flatpak client's --wake looks up the host's learned MAC (a no-op if none is
  // known), and the connect that follows has its own retry window, so a failure never blocks launch.
  const waking = wake(host, port).catch(() => ({ ok: false }));
  const { appId, runner } = await ensureStreamShortcut();
  const target = port && port !== 9777 ? `${host}:${port}` : host;
  const env = [`PF_HOST=${target}`];
  if (opts.browse) {
    env.push("PF_BROWSE=1");
    if (opts.mgmt) {
      env.push(`PF_MGMT=${Math.floor(opts.mgmt)}`);
    }
  } else if (opts.launchId) {
    if (!isSafeLaunchId(opts.launchId)) {
      // Enforced at pin time too (the picker disables Pin) — this is the backstop.
      throw new Error(`unsupported launch id: ${opts.launchId}`);
    }
    env.push(`PF_LAUNCH=${opts.launchId}`);
  }
  // KEY=value ... %command% args — %command% expands to the shortcut exe (/bin/sh); the wrapper
  // script rides behind it as an argument and reads PF_* from the environment.
  SteamClient.Apps.SetAppLaunchOptions(appId, `${env.join(" ")} %command% "${runner}"`);
  await waking; // ensure the magic packet is out before the connect attempt
  SteamClient.Apps.RunGame(gameIdFromAppId(appId), "", -1, 100);
}

/** Stop the running stream shortcut (best-effort; the in-stream chord/back also works). */
export function stopStream(): void {
  const appId = recall(STORAGE_KEY_STREAM);
  if (appId != null) {
    SteamClient.Apps.TerminateApp(gameIdFromAppId(appId), false);
  }
}
