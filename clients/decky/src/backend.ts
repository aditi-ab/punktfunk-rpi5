// Bridge to the Python backend (main.py) + shared types.
import { callable } from "@decky/api";

export interface Host {
  name: string;
  host: string;
  port: number;
  pair: string; // "required" | "optional" — the HOST's policy
  fp: string; // host cert SHA-256 fingerprint (lowercase hex) from the mDNS advert
  proto: string; // advertised protocol, e.g. "punktfunk/1"
  paired: boolean; // whether THIS device has already PIN-paired this host (by fingerprint)
  id: string; // the host's stable instance id (mDNS TXT `id`; "" when not advertised)
  mgmt: number; // management-API port (mDNS TXT `mgmt`; 0 = not advertised → default 47990)
}

// One title from a host's game library (the flatpak client's --library TSV, parsed by the
// backend). `id` is store-qualified (steam:<appid> / custom:<id>) and doubles as the
// launch handle (PF_LAUNCH → the session Hello).
export interface GameEntry {
  id: string;
  store: string; // "steam" | "custom" | "heroic" | "lutris" | …
  title: string;
}

export interface LibraryResult {
  ok: boolean;
  games?: GameEntry[];
  // "flatpak-not-found" | "timeout" | "not-paired" | "pin-mismatch" | "unreachable" |
  // "http" | "client-outdated" | "client-error"
  error?: string;
  detail?: string; // the client's own one-line reason, for the generic error copy
}

// A pinned game — a one-tap stream row in the QAM. The host is identified primarily by
// cert fingerprint (survives IP changes; pairing is fp-keyed too), with the stored
// address as the launch fallback when the host isn't currently advertising.
export interface PinnedGame {
  game_id: string;
  title: string;
  store: string;
  host_fp: string;
  host_id: string;
  host_name: string;
  host: string;
  port: number;
  mgmt: number;
  added_at: number; // unix seconds
  paired?: boolean; // annotated by get_pins from the client's known-hosts store
}

export interface PairResult {
  ok: boolean;
  fp?: string;
  error?: string;
}

export interface RunnerInfo {
  runner: string; // absolute path to bin/punktfunkrun.sh
  app_id: string; // flatpak app id
  exists: boolean;
}

// The slice of the flatpak client's settings JSON this UI surfaces. The file can hold more
// keys (codec, decoder, … set from the desktop client's own UI) — they round-trip untouched
// because get_settings returns the whole parsed file and patches are object spreads.
export interface StreamSettings {
  width: number; // 0 = native
  height: number; // 0 = native
  refresh_hz: number; // 0 = native
  bitrate_kbps: number; // 0 = host default
  gamepad: string; // "auto" | "xbox360" | "xboxone" | "dualsense" | "dualshock4" | "steamdeck"
  compositor: string; // "auto" | "kwin" | "wlroots" | "mutter" | "gamescope"
  inhibit_shortcuts: boolean;
  mic_enabled: boolean;
}

export interface UpdateInfo {
  current: string; // installed PLUGIN version (package.json)
  latest: string; // newest plugin version in our registry for this channel
  artifact: string; // immutable zip URL Decky should install
  hash: string; // sha256 of that zip (Decky verifies it)
  channel: string; // "latest" (stable) | "canary"
  update_available: boolean; // a newer PLUGIN build is available
  // The flatpak CLIENT (io.unom.Punktfunk) versions independently and is a per-user install, so
  // `sudo flatpak update` never touches it — the plugin offers a user-scope update instead.
  client_update_available: boolean;
  client_current: string; // installed client commit (short) — informational
  client_latest: string; // remote client commit (short) — informational
  error?: string; // "update-channel-unknown" (dev build) | "fetch-failed"
}

// Steam-shortcut artwork (assets/ in the plugin dir): base64 PNGs keyed grid / gridwide /
// hero / logo, plus the icon's absolute path (SetShortcutIcon wants a file). Keys for
// missing files are absent.
export interface ShortcutArt {
  grid?: string;
  gridwide?: string;
  hero?: string;
  logo?: string;
  icon_path: string;
}

export const discover = callable<[], Host[]>("discover");
export const pair = callable<
  [host: string, port: number, pin: string, name: string],
  PairResult
>("pair");
// Fetch a paired host's game library (headless flatpak --library; can take seconds on a
// cold client start — show a spinner). Pass fp whenever known so the pin can't degrade.
export const library = callable<
  [host: string, mgmt_port: number, fp: string],
  LibraryResult
>("library");
export const getPins = callable<[], { pins: PinnedGame[] }>("get_pins");
export const setPins = callable<[pins: PinnedGame[]], { ok: boolean; error?: string }>(
  "set_pins",
);
export const runnerInfo = callable<[], RunnerInfo>("runner_info");
export const shortcutArt = callable<[], ShortcutArt>("shortcut_art");
export const getSettings = callable<[], StreamSettings>("get_settings");
export const setSettings = callable<[settings: StreamSettings], { ok: boolean }>(
  "set_settings",
);
export const killStream = callable<[], { ok: boolean }>("kill_stream");
// Send a Wake-on-LAN magic packet to a saved host (headless flatpak --wake) so a sleeping host is
// up by the time the stream connects. The MAC is looked up from the flatpak client's own
// known-hosts store; `ok: false` (no-op) when none has been learned yet. Fire before launching.
export const wake = callable<[host: string, port: number], { ok: boolean; error?: string }>(
  "wake",
);
export const checkUpdate = callable<[force: boolean], UpdateInfo>("check_update");
// Update the flatpak client in the user installation (`flatpak update --user -y io.unom.Punktfunk`).
export const updateClient = callable<
  [],
  { ok: boolean; updated: boolean; error?: string }
>("update_client");
