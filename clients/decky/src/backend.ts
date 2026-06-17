// Bridge to the Python backend (main.py) + shared types.
import { callable } from "@decky/api";

export interface Host {
  name: string;
  host: string;
  port: number;
  pair: string; // "required" | "optional"
  fp: string;
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

export interface StreamSettings {
  width: number; // 0 = native
  height: number; // 0 = native
  refresh_hz: number; // 0 = native
  bitrate_kbps: number; // 0 = host default
  gamepad: string; // "auto" | "xbox360" | "dualsense"
  compositor: string; // "auto" | "kwin" | "wlroots" | "mutter" | "gamescope"
  inhibit_shortcuts: boolean;
  mic_enabled: boolean;
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
