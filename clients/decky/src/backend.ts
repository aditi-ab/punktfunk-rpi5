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
  current: string; // installed version (package.json)
  latest: string; // newest version in our registry for this channel
  artifact: string; // immutable zip URL Decky should install
  hash: string; // sha256 of that zip (Decky verifies it)
  channel: string; // "latest" (stable) | "canary"
  update_available: boolean;
  error?: string; // "update-channel-unknown" (dev build) | "fetch-failed"
}

export const discover = callable<[], Host[]>("discover");
export const pair = callable<
  [host: string, port: number, pin: string, name: string],
  PairResult
>("pair");
export const runnerInfo = callable<[], RunnerInfo>("runner_info");
export const getSettings = callable<[], StreamSettings>("get_settings");
export const setSettings = callable<[settings: StreamSettings], { ok: boolean }>(
  "set_settings",
);
export const killStream = callable<[], { ok: boolean }>("kill_stream");
export const checkUpdate = callable<[force: boolean], UpdateInfo>("check_update");
