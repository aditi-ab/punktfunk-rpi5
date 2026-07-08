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

// A host in the SHARED saved-hosts store (client-known-hosts.json) — the same file the desktop
// client reads/writes, so add/rename/pair in either surface shows up in both. `online` comes
// from a mDNS-INDEPENDENT reachability probe (a Tailscale/VPN host isn't shown offline just
// because it doesn't advertise); `null` means reachability is unknown (probe skipped or a client
// too old for `--list-hosts`, which then also can't probe).
export interface SavedHost {
  name: string;
  addr: string;
  port: number;
  fp_hex: string; // host cert fingerprint (lowercase hex); "" for a not-yet-paired manual entry
  paired: boolean;
  mac: string[];
  last_used: number | null;
  online: boolean | null;
}

export interface HostsResult {
  ok: boolean;
  hosts: SavedHost[];
  probed: boolean;
  fallback?: boolean; // true when read straight off disk (client too old for --list-hosts)
}

// The result of a host-store mutation (add/edit/forget). `error` is a stable code:
// "client-unavailable" (flatpak missing) | "client-outdated" (client predates the mode) |
// "unreachable"/"http"/… (from the client) | "client-error" (generic; see `detail`).
export interface MutationResult {
  ok: boolean;
  error?: string;
  detail?: string;
}

export interface RunnerInfo {
  runner: string; // absolute path to bin/punktfunkrun.sh
  app_id: string; // flatpak app id
  exists: boolean;
}

// The slice of the flatpak client's settings JSON this UI surfaces. The file can hold more
// keys (decoder, … set from the desktop client's own UI) — they round-trip untouched
// because get_settings returns the whole parsed file and patches are object spreads.
export interface StreamSettings {
  width: number; // 0 = native
  height: number; // 0 = native
  refresh_hz: number; // 0 = native
  bitrate_kbps: number; // 0 = host default
  codec?: string; // "auto" | "hevc" | "h264" | "av1" — soft preference (absent in pre-codec files)
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
// ---- Shared saved-hosts store (the SAME client-known-hosts.json the desktop client owns) ----
// The saved hosts, each annotated with a live (mDNS-independent) `online` probe when `probe` is
// true. Falls back to a direct JSON read (no reachability) on a client too old for --list-hosts.
export const listHosts = callable<[probe: boolean], HostsResult>("list_hosts");
// Save a host by address (survives mDNS-blind networks). `fp` empty = unpaired placeholder to
// pair next; a later pair replaces it with the fingerprinted entry.
export const addHost = callable<[target: string, name: string, fp: string], MutationResult>(
  "add_host",
);
// Rename and/or re-point a saved host. `selector` = its fingerprint (survives IP change) or
// current addr[:port]; empty fields are left untouched.
export const editHost = callable<
  [selector: string, name: string, addr: string, port: number],
  MutationResult
>("edit_host");
// Remove a saved host by fingerprint or addr[:port] (idempotent).
export const forgetHost = callable<[selector: string], MutationResult>("forget_host");
// Reset this device's Punktfunk state (saved hosts + stream settings + pins); KEEPS the client
// identity so the box isn't seen as new everywhere (re-pairing re-adds hosts).
export const resetConfig = callable<[], { ok: boolean; error?: string }>("reset_config");
// Reachability of one host[:port] via the client's mDNS-independent QUIC probe (a "test address"
// check). `{ ok: true, online }` when determined, else `{ ok: false, error }`.
export const probeHost = callable<
  [target: string],
  { ok: boolean; online?: boolean; error?: string }
>("probe_host");
export const checkUpdate = callable<[force: boolean], UpdateInfo>("check_update");
// Update the flatpak client in the user installation (`flatpak update --user -y io.unom.Punktfunk`).
export const updateClient = callable<
  [],
  { ok: boolean; updated: boolean; error?: string }
>("update_client");
