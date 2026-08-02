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
  os: string; // OS-identity chain (mDNS TXT `os`, e.g. "linux/fedora/bazzite"); "" on older hosts
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
  // OS-identity chain learned by the desktop client; optional because the installed
  // flatpak client may predate the field.
  os?: string;
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
  // Which client the backend resolved: the flatpak, a native install (.deb/rpm/sysext/AUR/nix),
  // or none at all. Older backends send neither field — hence optional.
  client_kind?: "flatpak" | "native" | "none";
  // Absolute path of the native binary; "" for flatpak. Passed to the wrapper as PF_CLIENT_BIN.
  client_bin?: string;
}

// The flatpak client's settings JSON — the SAME `client-gtk-settings.json` the desktop client
// and the console's settings screen own, so a value changed in any of them shows in the others.
//
// Every field the client's `Settings` struct persists is modelled here EXCEPT the four that
// cannot be answered from a plugin backend or aren't settings at all:
//   • `forward_pad`   — which physical pad is player 1. Needs SDL's live device list, which only
//                       the client process has; there is no CLI that enumerates pads.
//   • `last_window_w/h` — the session's remembered window size, written BY the client, not a
//                       preference anyone sets.
// Both round-trip untouched: get_settings returns the whole parsed file, patches are object
// spreads, and set_settings merges onto what's on disk.
//
// Optional (`?`) marks a key the client writes with a serde `default`, so a store written before
// that key existed simply lacks it. Read those through the same fallback the client uses —
// `?? true` for the default-on ones, never `!!` — or a pre-existing file reads as "off" here
// while the stream runs with it on.
export interface StreamSettings {
  // ---- Stream mode ----
  width: number; // 0 = native
  height: number; // 0 = native
  refresh_hz: number; // 0 = native
  render_scale?: number; // render-resolution multiplier; 1.0 = native (absent in pre-scale files)
  bitrate_kbps: number; // 0 = host default
  compositor: string; // "auto" | "kwin" | "wlroots" | "mutter" | "gamescope"
  // Stream mode follows the session window instead of width/height, renegotiating on resize.
  // Overrides width/height while on; degenerates to the display's native mode on fullscreen.
  match_window?: boolean;

  // ---- Video ----
  codec?: string; // "auto" | "hevc" | "h264" | "av1" | "pyrowave" (absent in pre-codec files)
  decoder?: string; // "auto" | "vulkan" | "vaapi" | "software"
  hdr_enabled?: boolean; // default ON — advertise 10-bit/HDR10
  enable_444?: boolean; // default off — ask for full chroma
  adapter?: string; // decode/present GPU by marketing name; "" = automatic

  // ---- Audio ----
  audio_channels?: number; // 2 (stereo) | 6 (5.1) | 8 (7.1)
  speaker_device?: string; // PipeWire node.name for playback; "" = system default
  mic_enabled: boolean;
  mic_device?: string; // PipeWire node.name for capture; "" = system default
  echo_cancel?: boolean; // default ON; only meaningful while mic_enabled

  // ---- Controllers ----
  gamepad: string; // "auto" | "xbox360" | "xboxone" | "dualsense" | "dualshock4" | "steamdeck"
  // Forward this device's controllers at all. Absent in pre-forwarding files, where the
  // client's own serde default (true) applies — so `?? true` at every read, never `!!`.
  gamepad_forwarding?: boolean;

  // ---- Touchscreen, mouse & keyboard ----
  touch_mode?: string; // "trackpad" | "pointer" | "touch"
  mouse_mode?: string; // "capture" | "desktop"
  invert_scroll?: boolean;
  // Whether the session grabs the keyboard so Alt+Tab/Super reach the host.
  inhibit_shortcuts: boolean;

  // ---- Interface & behaviour ----
  // Stats-overlay tier: "off" | "compact" | "normal" | "detailed". Absent in a pre-tier file,
  // which resolves through `show_stats` — read both the way the client's
  // `Settings::stats_verbosity` does, and write both the way `set_stats_verbosity` does.
  stats_verbosity?: string;
  // The legacy on/off the tier supersedes; kept written in sync so a client that predates the
  // tiers still honours an Off chosen here.
  show_stats?: boolean;
  fullscreen_on_stream?: boolean;
  auto_wake?: boolean; // default ON — Wake-on-LAN a sleeping host before connecting
  library_enabled?: boolean; // the CLIENT's own library browser (this plugin has its own)
}

// One audio endpoint from the client's enumeration: the stable id that gets stored, plus the
// human name to show.
export interface AudioDevice {
  name: string; // PipeWire node.name — what `speaker_device` / `mic_device` store
  description: string; // human label ("Steam Deck Speakers")
}

// What the device pickers need, read from the session binary (`--list-adapters` / `--list-audio`).
// `ok: false` = the session binary couldn't be run or failed; every list is then empty and the
// pickers stay on their stored value rather than pretending the device is gone.
export interface DeviceLists {
  ok: boolean;
  adapters: string[]; // Vulkan physical devices, discrete first
  sinks: AudioDevice[]; // playback endpoints
  sources: AudioDevice[]; // capture endpoints
}

export interface UpdateInfo {
  current: string; // installed PLUGIN version (package.json)
  latest: string; // newest plugin version in our registry for this channel
  artifact: string; // immutable zip URL Decky should install
  hash: string; // sha256 of that zip (Decky verifies it)
  channel: string; // "latest" (stable) | "canary"
  update_available: boolean; // a newer PLUGIN build is available
  // The CLIENT versions independently of this plugin, and how it updates depends on how it was
  // installed. A flatpak is a per-user install `sudo flatpak update` never touches, compared by
  // OSTree commit; every other install (.deb/.rpm/pacman/sysext/nix/source) is compared by the
  // client itself against the signed per-channel manifest (`punktfunk-client --check-update`).
  client_update_available: boolean;
  client_current: string; // installed client commit (flatpak) or version (native)
  client_latest: string; // newest client commit (flatpak) or version (native)
  client_install: string; // "flatpak" | "apt" | "dnf" | "pacman" | "sysext" | "nix" | "source" | ""
  // Who can perform the update: "flatpak" (this plugin runs it), "helper" (the client drives the
  // packaged root helper), "none" (nothing here can — show `client_command`).
  client_applier: string;
  client_command: string; // one copy-pastable line that updates this install by hand
  client_opt_in: string; // set when one-tap WOULD work after `usermod -aG punktfunk-update`
  client_error?: string; // the client check couldn't complete (e.g. "client-outdated")
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
// Install the Steam Input layout (native touchscreen `ts_n` + gamepad passthrough) and point our
// shortcut(s) at it, so the Deck touchscreen reaches the client as native touch with no manual
// controller setup. Best-effort + idempotent; keyed by the shared shortcut NAME (both shortcuts
// use the same name → the same lowercase configset key), so one call covers both.
export const applyControllerConfig = callable<
  [name: string],
  { ok: boolean; applied?: string[]; errors?: string[]; accounts?: number; error?: string; detail?: string }
>("apply_controller_config");
export const getSettings = callable<[], StreamSettings>("get_settings");
export const setSettings = callable<[settings: StreamSettings], { ok: boolean }>(
  "set_settings",
);
// GPUs + audio endpoints for the device pickers. Costs a subprocess that initialises Vulkan and
// PipeWire, so it is called ONCE when the settings tab mounts and never on the launch path.
export const listDevices = callable<[], DeviceLists>("list_devices");
// The same, bypassing the backend's cache — for the user who just plugged in a headset.
export const refreshDevices = callable<[], DeviceLists>("refresh_devices");
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
// Update the client by whichever route its install supports: `flatpak update --user` for the
// flatpak, `punktfunk-client --apply-update` (the packaged root helper) for a one-tap-capable
// native install. Everything else comes back `ok: false, error: "manual"` with `command` — the
// line to run by hand. A package-manager run can take minutes; the backend allows 15.
export const updateClient = callable<
  [],
  {
    ok: boolean;
    updated: boolean;
    staged?: boolean; // installed, but a reboot activates it (rpm-ostree)
    error?: string;
    detail?: string;
    command?: string; // set with error "manual"
  }
>("update_client");
