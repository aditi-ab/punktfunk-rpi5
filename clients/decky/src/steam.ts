// Launch the stream as a Steam game so gamescope focuses + fullscreens it.
//
// THE LAUNCH MECHANISM (verified against MoonDeck): gamescope only gives focus/fullscreen to
// the window tree Steam launched via `reaper` (it detects the "current app" by AppID — see
// gamescope#484). So we cannot launch the flatpak from the plugin backend; we register ONE
// hidden non-Steam shortcut that points at our wrapper script (bin/punktfunkrun.sh), pass the
// per-session host as the shortcut's Steam launch options, and start it with RunGame. The
// wrapper then execs `flatpak run io.unom.Punktfunk --connect <host>` as a reaper descendant.

import { runnerInfo } from "./backend";

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
    SetAppLaunchOptions(appId: number, options: string): void;
    RunGame(gameId: string, _unused: string, _i: number, _j: number): void;
    TerminateApp(gameId: string, _b: boolean): void;
  };
};

// Steam removed `SteamClient.Apps.SetAppHidden`. Hiding a non-Steam shortcut now goes through
// `collectionStore.SetAppsAsHidden([appId], true)` — but that looks the app up in appStore, which
// only registers a freshly-created shortcut a moment later (calling it immediately throws on a
// null overview). So hiding is BEST-EFFORT + DEFERRED and must NEVER block the launch.
declare const collectionStore:
  | { SetAppsAsHidden?: (appIds: number[], hidden: boolean) => void }
  | undefined;

function hideShortcut(appId: number): void {
  const attempt = () => {
    try {
      collectionStore?.SetAppsAsHidden?.([appId], true);
    } catch {
      /* overview not registered yet, or the API changed — cosmetic, ignore */
    }
  };
  attempt(); // succeeds immediately for an already-registered (reused) shortcut
  setTimeout(attempt, 2500); // fresh shortcut: retry once its app overview lands
}

const SHORTCUT_NAME = "punktfunk";

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
 * Ensure exactly one hidden "punktfunk" shortcut exists pointing at the wrapper script, and
 * return its appId. Reuses the remembered one when its exe still matches the current runner
 * path (the plugin dir can change across reinstalls).
 */
async function ensureShortcut(): Promise<number> {
  const info = await runnerInfo();
  if (!info.exists) {
    throw new Error(`launch wrapper missing at ${info.runner}`);
  }

  const remembered = recallAppId();
  if (remembered != null) {
    // Re-point the existing shortcut at the current runner path (cheap + idempotent).
    SteamClient.Apps.SetShortcutExe(remembered, info.runner);
    SteamClient.Apps.SetShortcutStartDir(
      remembered,
      info.runner.replace(/\/[^/]*$/, ""),
    );
    return remembered;
  }

  const appId = await SteamClient.Apps.AddShortcut(
    SHORTCUT_NAME,
    info.runner,
    info.runner.replace(/\/[^/]*$/, ""), // start dir = the bin/ dir
    "",
  );
  SteamClient.Apps.SetShortcutName(appId, SHORTCUT_NAME);
  // Hide it from the library — it's an implementation detail, launched programmatically.
  // Best-effort + deferred (see hideShortcut); never let it block the launch.
  hideShortcut(appId);
  rememberAppId(appId);
  return appId;
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

/**
 * Launch a stream to `host:port` fullscreen in Gaming Mode. Encodes the target into the
 * shortcut's launch options (so one generic shortcut serves every host), then RunGame.
 */
export async function launchStream(host: string, port: number): Promise<void> {
  const appId = await ensureShortcut();
  // Best-effort so the Deck's rich controls reach the client; no-op if the API is absent (the user
  // disables Steam Input manually — see the Settings instruction).
  disableSteamInputForShortcut(appId);
  const target = port && port !== 9777 ? `${host}:${port}` : host;
  // KEY=value ... %command% — the wrapper reads PF_HOST from the environment.
  SteamClient.Apps.SetAppLaunchOptions(appId, `PF_HOST=${target} %command%`);
  SteamClient.Apps.RunGame(gameIdFromAppId(appId), "", -1, 100);
}

/** Stop the running stream shortcut (best-effort; the in-stream chord/back also works). */
export function stopStream(): void {
  const appId = recallAppId();
  if (appId != null) {
    SteamClient.Apps.TerminateApp(gameIdFromAppId(appId), false);
  }
}
