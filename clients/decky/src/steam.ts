// Launch the stream as a Steam game so gamescope focuses + fullscreens it.
//
// THE LAUNCH MECHANISM (verified against MoonDeck): gamescope only gives focus/fullscreen to
// the window tree Steam launched via `reaper` (it detects the "current app" by AppID — see
// gamescope#484). So we cannot launch the flatpak from the plugin backend; we register ONE
// hidden non-Steam shortcut whose exe is `/bin/sh` running our wrapper script
// (bin/punktfunkrun.sh), pass the per-session host as the shortcut's Steam launch options,
// and start it with RunGame. The wrapper then execs
// `flatpak run io.unom.Punktfunk --connect <host>` as a reaper descendant.

import { runnerInfo, shortcutArt } from "./backend";

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

// The shortcut used to be hidden ("implementation detail"); it is user-visible now — it
// carries proper artwork and living in the library is how users relaunch their last host.
// Existing installs still have theirs hidden, so unhide is applied every ensure (idempotent).
function unhideShortcut(appId: number): void {
  const attempt = () => {
    try {
      collectionStore?.SetAppsAsHidden?.([appId], false);
    } catch {
      /* overview not registered yet, or the API changed — cosmetic, ignore */
    }
  };
  attempt(); // succeeds immediately for an already-registered (reused) shortcut
  setTimeout(attempt, 2500); // fresh shortcut: retry once its app overview lands
}

// Bump when the shipped artwork changes so existing shortcuts re-apply it once.
const ART_VERSION = 1;
const ART_KEY = "punktfunk:shortcutArt";

/**
 * Apply the plugin's grid/hero/logo/icon to the shortcut (idempotent, once per ART_VERSION).
 * Cosmetic and fully best-effort: any failure is swallowed and retried on the next launch.
 */
async function applyArtwork(appId: number): Promise<void> {
  try {
    if (localStorage.getItem(ART_KEY) === `${appId}:${ART_VERSION}`) {
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
    localStorage.setItem(ART_KEY, `${appId}:${ART_VERSION}`);
  } catch (e) {
    console.warn("punktfunk: shortcut artwork not applied", e);
  }
}

// The shortcut name is user-visible (Steam overlay + library while streaming) — brand-case it.
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

// Persist our shortcut appId across reloads so we reuse ONE shortcut instead of churning the
// library (the appId is stable for the life of the shortcut).
const STORAGE_KEY = "punktfunk:shortcutAppId";

function rememberAppId(appId: number) {
  try {
    localStorage.setItem(STORAGE_KEY, String(appId));
  } catch {
    /* ignore */
  }
}
function recallAppId(): number | null {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    return v ? Number(v) : null;
  } catch {
    return null;
  }
}

/**
 * Ensure exactly one "Punktfunk" shortcut exists (exe = /bin/sh; the wrapper script is
 * appended per-launch via the launch options), branded and visible in the library, and
 * return its appId + the current runner path. Reuses the remembered shortcut, re-pointing
 * it each time — the plugin dir can change across reinstalls, pre-0.4 shortcuts pointed at
 * the script directly, and pre-0.7 shortcuts were hidden and artless.
 */
async function ensureShortcut(): Promise<{ appId: number; runner: string }> {
  const info = await runnerInfo();
  if (!info.exists) {
    throw new Error(`launch wrapper missing at ${info.runner}`);
  }
  const startDir = info.runner.replace(/\/[^/]*$/, ""); // the plugin's bin/ dir

  const remembered = recallAppId();
  if (remembered != null) {
    // Re-point + rename the existing shortcut (cheap + idempotent — migrates old installs).
    SteamClient.Apps.SetShortcutExe(remembered, SHELL);
    SteamClient.Apps.SetShortcutStartDir(remembered, startDir);
    SteamClient.Apps.SetShortcutName(remembered, SHORTCUT_NAME);
    unhideShortcut(remembered); // pre-0.7 installs hid it
    void applyArtwork(remembered); // fire-and-forget — cosmetic, never blocks the launch
    return { appId: remembered, runner: info.runner };
  }

  const appId = await SteamClient.Apps.AddShortcut(SHORTCUT_NAME, SHELL, startDir, "");
  SteamClient.Apps.SetShortcutName(appId, SHORTCUT_NAME);
  unhideShortcut(appId);
  void applyArtwork(appId); // fire-and-forget — cosmetic, never blocks the launch
  rememberAppId(appId);
  return { appId, runner: info.runner };
}

/**
 * Best-effort: turn Steam Input OFF for our shortcut so SDL's HIDAPI Steam Deck driver can open the
 * Deck's controls (paddles · trackpads · gyro) directly. There is no confirmed-stable SteamClient
 * API for this, so it is feature-detected and MUST never block or throw into the launch — the manual
 * toggle (game page → ⚙ → Controller Settings → Steam Input Off, surfaced in the plugin Settings) is
 * the documented source of truth. No-op when the optional API is absent.
 */
function disableSteamInputForShortcut(appId: number): void {
  try {
    const input = (
      SteamClient as unknown as {
        Input?: { SetSteamInputEnabledForApp?: (appId: number, enabled: boolean) => void };
      }
    ).Input;
    input?.SetSteamInputEnabledForApp?.(appId, false);
  } catch {
    /* a controller tweak must never break the launch */
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
 * library title, or into the gamepad library launcher). Encodes the target into the
 * shortcut's launch options (so one generic shortcut serves every host and every pinned
 * game), then RunGame.
 */
export async function launchStream(
  host: string,
  port: number,
  opts: LaunchOpts = {},
): Promise<void> {
  const { appId, runner } = await ensureShortcut();
  // Best-effort so the Deck's rich controls reach the client; no-op if the API is absent (the user
  // disables Steam Input manually — see the Settings instruction).
  disableSteamInputForShortcut(appId);
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
  SteamClient.Apps.RunGame(gameIdFromAppId(appId), "", -1, 100);
}

/** Stop the running stream shortcut (best-effort; the in-stream chord/back also works). */
export function stopStream(): void {
  const appId = recallAppId();
  if (appId != null) {
    SteamClient.Apps.TerminateApp(gameIdFromAppId(appId), false);
  }
}
