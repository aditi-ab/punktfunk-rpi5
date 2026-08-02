// Stream settings — the client's WHOLE settings store, written to the JSON the client reads on
// launch (main.py set_settings, merged onto what's on disk). This is the same
// `client-gtk-settings.json` the desktop client and the console's settings screen own, so a value
// changed in any of the three shows in the other two.
//
// SHAPE OF THIS SCREEN. Thirty rows is too many to scroll past on a thumbstick, so they are split
// across a `SidebarNavigation` — the same left-rail-of-categories layout SteamOS's own Settings
// uses, and the one Deck users already know. Every page fits on screen without scrolling, which is
// the whole point of the split: the rail is the index, so nothing is more than one hop away.
//
// The categories, their order, and the wording of the rows are the console's settings screen
// (pf-console-ui/src/screens/settings.rs) — that screen is the other settings editor a user
// reaches without leaving Gaming Mode, and two different orders for one store is how people stop
// trusting either. It shows them as one steppable list because it has no pointer and no room for
// a rail; here they become the rail's pages, same groups, same sequence. Three more rules:
//
//   • A setting that depends on another is INDENTED under it and DISABLED, never hidden — the
//     console dims those rows rather than dropping them, and a row that vanishes as you toggle
//     the one above it is a moving target for a thumbstick.
//   • A picker whose options this device doesn't have doesn't appear at all (the GPU row on a
//     one-GPU Deck). A dead control is worse than an absent one.
//   • Anything that behaves differently *here* than it does on a desktop says so in its own
//     description, rather than being silently dropped from the screen.
//
// The accepted gamepad/compositor/codec/decoder names mirror punktfunk-core's `*Pref::from_name`
// and the console's tables; the tier/mode names mirror the `StatsVerbosity` / `TouchMode` /
// `MouseMode` enums, which serialize lowercase.
import {
  DialogButton,
  Dropdown,
  Field,
  SidebarNavigation,
  SliderField,
  Spinner,
  ToggleField,
} from "@decky/ui";
import { CSSProperties, FC, ReactElement, ReactNode, useEffect, useState } from "react";
import {
  FaDesktop,
  FaGamepad,
  FaHandPointer,
  FaSlidersH,
  FaTv,
  FaVideo,
  FaVolumeUp,
} from "react-icons/fa";
import {
  AudioDevice,
  DeviceLists,
  getSettings,
  listDevices,
  refreshDevices,
  setSettings,
  StreamSettings,
} from "./backend";
import { actionButton, RowActions } from "./ui";

// Decky's Dropdown has no width prop — it fills whatever container it's in, and a
// `childrenContainerWidth="max"` Field is the whole row. Wrapping it in this fit-content shell
// (inside the right-aligned RowActions) shrinks the control to its selected label, with a floor
// so short values like "60 Hz" don't collapse to a nub and a ceiling so nothing runs edge to
// edge. Matches the right-aligned, content-sized buttons everywhere else.
const selectShell: CSSProperties = {
  width: "fit-content",
  minWidth: "10em",
  maxWidth: "24em",
};

// ----------------------------------------------------------------------------------------
// Option tables — the console's, so the two Gaming-Mode editors offer the same choices.
// ----------------------------------------------------------------------------------------

// "native" and "match" are virtual: they store `width`/`height` of 0 with `match_window` off/on.
// Match window is offered even though this plugin's launches are always fullscreen (where it
// degenerates to the display's native mode) — leaving it out would make the row lie about a
// store the desktop client can set it in.
const MATCH_WINDOW = "match";
const RESOLUTIONS: [number, number, string][] = [
  [0, 0, "Native display"],
  [1280, 720, "1280 × 720"],
  [1280, 800, "1280 × 800 (Deck)"],
  [1920, 1080, "1920 × 1080"],
  [2560, 1440, "2560 × 1440"],
  [3840, 2160, "3840 × 2160"],
];
const resolutionKey = (w: number, h: number): string => (w === 0 && h === 0 ? "native" : `${w}x${h}`);

const REFRESH = [0, 30, 60, 90, 120];
// Render-resolution multipliers (mirrors punktfunk_core::render_scale::PRESETS). 1.0 = native.
const RENDER_SCALES = [0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];
const renderScaleLabel = (x: number): string =>
  x === 1 ? "Native (1×)" : x > 1 ? `${x}× · supersample` : `${x}×`;

const COMPOSITORS: [string, string][] = [
  ["auto", "Automatic"],
  ["kwin", "KDE Plasma (KWin)"],
  ["wlroots", "Sway (wlroots)"],
  ["mutter", "GNOME (Mutter)"],
  ["gamescope", "gamescope"],
];
const CODECS: [string, string][] = [
  ["auto", "Automatic"],
  ["hevc", "HEVC (H.265)"],
  ["h264", "H.264 (AVC)"],
  ["av1", "AV1"],
  // Opt-in wired-LAN low-latency codec (100–400 Mbit/s class, 8-bit SDR). Only ever selected
  // when the host advertises it too; anything else falls back to HEVC.
  ["pyrowave", "PyroWave (wired LAN)"],
];
const DECODERS: [string, string][] = [
  ["auto", "Automatic"],
  ["vulkan", "Vulkan Video"],
  ["vaapi", "VAAPI"],
  ["software", "Software"],
];
// Presentation intent — the `present_priority` key shared with the Apple and Android clients, so
// one profile reads the same on every device.
const PRESENT_PRIORITIES: [string, string][] = [
  ["latency", "Lowest latency"],
  ["smooth", "Smoothness"],
];
// Smoothness buffer depth in frames; 0 = Automatic (resolves to 2).
const SMOOTH_BUFFERS: [number, string][] = [
  [0, "Automatic"],
  [1, "1 frame"],
  [2, "2 frames"],
  [3, "3 frames"],
];
const AUDIO_CHANNELS: [number, string][] = [
  [2, "Stereo"],
  [6, "5.1 surround"],
  [8, "7.1 surround"],
];
const GAMEPADS: [string, string][] = [
  ["auto", "Automatic"],
  ["xbox360", "Xbox 360"],
  ["xboxone", "Xbox One"],
  ["dualsense", "DualSense"],
  ["dualshock4", "DualShock 4"],
  ["steamdeck", "Steam Deck"],
];
const TOUCH_MODES: [string, string][] = [
  ["trackpad", "Trackpad"],
  ["pointer", "Direct pointer"],
  ["touch", "Touch passthrough"],
];
const MOUSE_MODES: [string, string][] = [
  ["capture", "Capture (games)"],
  ["desktop", "Desktop (absolute)"],
];
const STATS_TIERS: [string, string][] = [
  ["off", "Off"],
  ["compact", "Compact"],
  ["normal", "Normal"],
  ["detailed", "Detailed"],
];

// ----------------------------------------------------------------------------------------
// Row primitives — every picker row is Field + right-aligned, content-sized Dropdown, so the
// twelve of them below stay one line each and can't drift apart.
// ----------------------------------------------------------------------------------------

const SelectRow = <T extends string | number>({
  label,
  description,
  options,
  value,
  onChange,
  formatUnknown,
  disabled,
  indent,
}: {
  label: string;
  description?: ReactNode;
  options: [T, string][];
  value: T;
  onChange: (v: T) => void;
  // How to name a stored value this table doesn't list (see below); defaults to the raw value.
  formatUnknown?: (v: T) => string;
  disabled?: boolean;
  indent?: boolean;
}): ReactElement => {
  // A Dropdown can only display a value that is one of its options, and this store has four other
  // writers — the desktop client, the console, a settings profile, a newer client with presets
  // this build doesn't know. Rather than render a blank control (or, worse, silently show a
  // different value than the stream will actually use), carry the stored one as its own entry.
  const shown: [T, string][] = options.some(([v]) => v === value)
    ? options
    : [...options, [value, formatUnknown ? formatUnknown(value) : String(value)]];
  return (
    <Field
      label={label}
      description={description}
      disabled={disabled}
      indentLevel={indent ? 1 : undefined}
      childrenContainerWidth="max"
    >
      <RowActions>
        <div style={selectShell}>
          <Dropdown
            disabled={disabled}
            rgOptions={shown.map(([data, l]) => ({ data, label: l }))}
            selectedOption={value}
            onChange={(o) => onChange(o.data as T)}
          />
        </div>
      </RowActions>
    </Field>
  );
};

// An audio-endpoint picker. The stored value is a PipeWire `node.name`; "" means "whatever the OS
// is using". A stored endpoint that isn't in the current enumeration still gets an entry — it is
// a real preference that simply isn't plugged in right now, and dropping it would silently
// re-point the next stream at the default without ever showing the user why.
const DeviceRow: FC<{
  label: string;
  description: string;
  devices: AudioDevice[] | null;
  value: string;
  onChange: (v: string) => void;
  disabled?: boolean;
  indent?: boolean;
}> = ({ label, description, devices, value, onChange, disabled, indent }) => {
  const options: [string, string][] = [["", "System default"]];
  for (const d of devices ?? []) options.push([d.name, d.description]);
  if (value && !options.some(([name]) => name === value)) {
    options.push([value, `${value} (not connected)`]);
  }
  return (
    <SelectRow
      label={label}
      description={devices === null ? "Reading this device's audio endpoints…" : description}
      options={options}
      value={value}
      onChange={onChange}
      disabled={disabled || devices === null}
      indent={indent}
    />
  );
};

// ----------------------------------------------------------------------------------------
// The pages. One settings object, seven views on it — every page takes the same context rather
// than fetching or holding state of its own, so a change on one page is visible on the others
// the moment you switch.
// ----------------------------------------------------------------------------------------

interface PageCtx {
  s: StreamSettings;
  patch: (p: Partial<StreamSettings>) => void;
  devices: DeviceLists | null;
  reading: boolean;
  readDevices: (again: boolean) => void;
}

// SidebarNavigation gives each page Steam's own padding, but the routed page still renders
// UNDER Gaming Mode's footer hint bar, so the last row of a page needs to clear it (the same
// inset the tabs use).
const pageBody: CSSProperties = { paddingBottom: "80px" };

const StreamPage: FC<PageCtx> = ({ s, patch }) => {
  const renderScale = s.render_scale ?? 1;
  const resolution = s.match_window ? MATCH_WINDOW : resolutionKey(s.width, s.height);
  return (
    <div style={pageBody}>
      <SelectRow
        label="Resolution"
        description="The host creates a virtual display at exactly this size — no scaling. Match window follows the stream window instead, which in Gaming Mode means the Deck's native size."
        options={[
          ...RESOLUTIONS.map(([w, h, label]) => [resolutionKey(w, h), label] as [string, string]),
          [MATCH_WINDOW, "Match window"] as [string, string],
        ]}
        value={resolution}
        // A size set from a desktop profile that isn't one of these presets, spelled the way the
        // presets are rather than left as the raw "1600x900" key.
        formatUnknown={(v) => v.replace("x", " × ")}
        onChange={(v) => {
          if (v === MATCH_WINDOW) {
            // The tri-state the console stores: the flag on, the explicit size cleared.
            patch({ match_window: true, width: 0, height: 0 });
            return;
          }
          const found = RESOLUTIONS.find(([w, h]) => resolutionKey(w, h) === v);
          patch({ match_window: false, width: found?.[0] ?? 0, height: found?.[1] ?? 0 });
        }}
      />
      <SelectRow
        label="Refresh rate"
        description="Native follows the display the stream is on."
        options={REFRESH.map((r) => [r, r === 0 ? "Native" : `${r} Hz`] as [number, string])}
        value={s.refresh_hz}
        formatUnknown={(v) => `${v} Hz`}
        onChange={(v) => patch({ refresh_hz: v })}
      />
      <SelectRow
        label="Render scale"
        description="The host renders larger or smaller than the stream mode and the Deck resamples — above 1× supersamples for sharpness, below 1× saves bandwidth."
        options={RENDER_SCALES.map((x) => [x, renderScaleLabel(x)] as [number, string])}
        // Snap the stored value to the nearest preset so the dropdown always shows a match.
        value={RENDER_SCALES.reduce((best, x) =>
          Math.abs(x - renderScale) < Math.abs(best - renderScale) ? x : best,
        )}
        onChange={(v) => patch({ render_scale: v })}
      />
      <SliderField
        label="Bitrate"
        description="0 = the host's own default (20 Mbit/s)."
        value={Math.round(s.bitrate_kbps / 1000)}
        min={0}
        max={150}
        step={5}
        showValue
        valueSuffix=" Mbit/s"
        onChange={(v) => patch({ bitrate_kbps: v * 1000 })}
      />
      <SelectRow
        label="Host compositor"
        description="Which compositor drives the virtual display — honoured only if it's available on the host. Automatic suits almost every host."
        options={COMPOSITORS}
        value={s.compositor}
        onChange={(v) => patch({ compositor: v })}
      />
    </div>
  );
};

const VideoPage: FC<PageCtx> = ({ s, patch, devices }) => {
  // Only worth a row on a box that actually has a choice to make. A Deck has one adapter, and a
  // picker with a single option is a control that can't do anything.
  const showGpuRow = (devices?.adapters.length ?? 0) > 1;
  return (
    <div style={pageBody}>
      <SelectRow
        label="Video codec"
        description="A preference — the host falls back when its GPU can't encode this one."
        options={CODECS}
        value={s.codec ?? "auto"}
        onChange={(v) => patch({ codec: v })}
      />
      <SelectRow
        label="Video decoder"
        description="How the Deck decodes the stream. Automatic prefers Vulkan Video, then VAAPI, then software."
        options={DECODERS}
        value={s.decoder ?? "auto"}
        onChange={(v) => patch({ decoder: v })}
      />
      {showGpuRow && (
        <SelectRow
          label="Decode GPU"
          description="Which adapter decodes and presents the stream. Automatic picks the discrete GPU where there is one."
          options={[
            ["", "Automatic"],
            ...(devices?.adapters ?? []).map((a) => [a, a] as [string, string]),
          ]}
          value={s.adapter ?? ""}
          onChange={(v) => patch({ adapter: v })}
        />
      )}
      <ToggleField
        label="10-bit HDR"
        description="Advertise HDR10 so the host sends 10-bit when the content is HDR. Off means never ask for 10-bit."
        checked={s.hdr_enabled ?? true}
        onChange={(v) => patch({ hdr_enabled: v })}
      />
      <ToggleField
        label="Full chroma (4:4:4)"
        description="Full-colour video: crisp small text and thin lines, at more bandwidth. Needs an NVIDIA host (NVENC) or the PyroWave codec — other encoders stream 4:2:0 and the session falls back silently."
        checked={s.enable_444 ?? false}
        onChange={(v) => patch({ enable_444: v })}
      />
    </div>
  );
};

const PresentationPage: FC<PageCtx> = ({ s, patch }) => {
  const smooth = (s.present_priority ?? "latency") === "smooth";
  return (
    <div style={pageBody}>
      <SelectRow
        label="Prioritize"
        description="What to optimise for when a decoded frame is ready. Lowest latency shows each frame the moment the display can take it — a network hiccup becomes an occasional repeated or skipped frame. Smoothness buffers a little to even those out."
        options={PRESENT_PRIORITIES}
        value={s.present_priority ?? "latency"}
        onChange={(v) => patch({ present_priority: v })}
      />
      <SelectRow
        label="Smoothness buffer"
        description="Frames held back before showing. Each one absorbs about a refresh of network hiccup and adds a refresh of delay. Automatic holds two."
        options={SMOOTH_BUFFERS}
        value={s.smooth_buffer ?? 0}
        formatUnknown={(v) => `${v} frames`}
        onChange={(v) => patch({ smooth_buffer: v })}
        disabled={!smooth}
        indent
      />
      <ToggleField
        label="V-Sync"
        description="Tear-free. Off removes the wait for the screen's refresh — the lowest possible delay, at the cost of visible tearing. Best-effort: not every driver offers it, and the Detailed stats overlay names the mode actually in use."
        checked={s.vsync ?? true}
        onChange={(v) => patch({ vsync: v })}
      />
      <ToggleField
        label="Follow variable refresh"
        description="On a VRR screen, let the panel refresh in step with the stream instead of on a fixed cadence. Applies to fullscreen sessions — which a Gaming-Mode stream always is — and is harmless on a fixed-refresh screen."
        checked={s.allow_vrr ?? true}
        onChange={(v) => patch({ allow_vrr: v })}
      />
    </div>
  );
};

const AudioPage: FC<PageCtx> = ({ s, patch, devices, reading, readDevices }) => {
  const micOn = s.mic_enabled;
  // What the pickers get: null while the enumeration is in flight (they show a loading state),
  // [] when it answered but couldn't read the endpoints (System default plus whatever is
  // stored), and the real list otherwise.
  const endpoints = (list: AudioDevice[] | undefined): AudioDevice[] | null =>
    reading || !devices ? null : devices.ok ? (list ?? []) : [];
  return (
    <div style={pageBody}>
      <SelectRow
        label="Audio channels"
        description="The speaker layout requested from the host, which clamps it to what it can capture."
        options={AUDIO_CHANNELS}
        value={s.audio_channels ?? 2}
        formatUnknown={(v) => `${v} channels`}
        onChange={(v) => patch({ audio_channels: v })}
      />
      <DeviceRow
        label="Output device"
        description="Where stream audio plays. System default follows whatever the Deck is using, including a headset you plug in mid-stream."
        devices={endpoints(devices?.sinks)}
        value={s.speaker_device ?? ""}
        onChange={(v) => patch({ speaker_device: v })}
      />
      <ToggleField
        label="Stream microphone"
        description="Send the Deck's microphone to the host's virtual mic. Ctrl+Alt+Shift+V mutes and unmutes it mid-stream."
        checked={micOn}
        onChange={(v) => patch({ mic_enabled: v })}
      />
      <DeviceRow
        label="Microphone device"
        description="Which input the mic uplink captures from."
        devices={endpoints(devices?.sources)}
        value={s.mic_device ?? ""}
        onChange={(v) => patch({ mic_device: v })}
        disabled={!micOn}
        indent
      />
      <ToggleField
        label="Echo cancellation"
        description="Stops the host's audio, playing from the Deck's speakers, being picked up and sent back. Turn it off if your microphone already runs its own processing."
        checked={s.echo_cancel ?? true}
        onChange={(v) => patch({ echo_cancel: v })}
        disabled={!micOn}
        indentLevel={1}
      />
      {/* The escape hatch for a headset plugged in after this page was opened, and the honest
          answer when the enumeration failed outright (a client too old to ship the session
          binary). Rendered unconditionally, including while it is reading: a row that comes and
          goes under a thumbstick is a moving target, so only its wording changes. */}
      <Field
        label={
          !reading && devices && !devices.ok ? "Couldn't read this device's hardware" : "Devices"
        }
        description={
          reading
            ? "Reading this device's audio endpoints and GPUs…"
            : devices && !devices.ok
              ? "The output, microphone and GPU pickers fall back to Automatic. Reading them needs the client's session binary, which a client older than the two-binary split doesn't ship — update it from the About tab."
              : "Plugged something in just now? Read the audio endpoints and GPUs again."
        }
        childrenContainerWidth="max"
      >
        <RowActions>
          <DialogButton style={actionButton} disabled={reading} onClick={() => readDevices(true)}>
            {reading ? <Spinner style={{ height: "1em" }} /> : "Refresh"}
          </DialogButton>
        </RowActions>
      </Field>
    </div>
  );
};

const ControllersPage: FC<PageCtx> = ({ s, patch }) => {
  const forwarding = s.gamepad_forwarding ?? true;
  return (
    <div style={pageBody}>
      <ToggleField
        label="Forward controllers"
        description="Send controllers connected to the Deck to the host. Turn it off when your controller already reaches the host another way — USB passthrough such as VirtualHere, or a pad plugged into the host — so games don't see two of them."
        checked={forwarding}
        onChange={(v) => patch({ gamepad_forwarding: v })}
      />
      <SelectRow
        label="Controller type"
        description="The virtual pad the host creates. Automatic matches the controller you're holding."
        options={GAMEPADS}
        value={s.gamepad}
        onChange={(v) => patch({ gamepad: v })}
        disabled={!forwarding}
        indent
      />
      {forwarding && (s.gamepad === "steamdeck" || s.gamepad === "auto") && (
        <Field
          label="⚠ Disable Steam Input"
          description="On a Deck, Automatic forwards the built-in controller as a Steam Deck pad — paddles, both trackpads, and gyro included. For that, Steam Input must be OFF for Punktfunk: on the game page tap ⚙ → Controller Settings → set Steam Input to Off. Otherwise Steam keeps the Deck's controls and only the sticks + buttons reach the host."
          indentLevel={1}
        />
      )}
    </div>
  );
};

const PointerPage: FC<PageCtx> = ({ s, patch }) => (
  <div style={pageBody}>
    <SelectRow
      label="Touch mode"
      description="How the touchscreen drives the host: Trackpad (relative cursor, tap to click), Direct pointer (the cursor jumps to your finger), or Touch passthrough (every finger is a host contact — only helps apps that understand touch)."
      options={TOUCH_MODES}
      value={s.touch_mode ?? "trackpad"}
      onChange={(v) => patch({ touch_mode: v })}
    />
    <SelectRow
      label="Mouse mode"
      description="How a physical mouse drives the host: Capture locks the pointer for games, Desktop leaves it free and sends absolute positions. Ctrl+Alt+Shift+M switches it live mid-stream."
      options={MOUSE_MODES}
      value={s.mouse_mode ?? "capture"}
      onChange={(v) => patch({ mouse_mode: v })}
    />
    <ToggleField
      label="Invert scroll direction"
      description="Reverses the wheel and trackpad scroll direction sent to the host."
      checked={s.invert_scroll ?? false}
      onChange={(v) => patch({ invert_scroll: v })}
    />
    <ToggleField
      label="Capture system shortcuts"
      description="Sends Alt+Tab, Super and friends to the host while input is captured, instead of leaving them to the local desktop. Gaming Mode is gamescope, which has no shortcuts to hold back — this is for a keyboard attached to the Deck in Desktop Mode, and for the desktop client sharing these settings."
      checked={s.inhibit_shortcuts}
      onChange={(v) => patch({ inhibit_shortcuts: v })}
    />
  </div>
);

const InterfacePage: FC<PageCtx> = ({ s, patch }) => {
  // `Settings::stats_verbosity`: no tier = a pre-tier store, resolved through the legacy bool,
  // which itself defaults to true.
  const statsTier = s.stats_verbosity ?? ((s.show_stats ?? true) ? "normal" : "off");
  return (
    <div style={pageBody}>
      <SelectRow
        label="Statistics overlay"
        description="How much the in-stream overlay shows: Compact (fps · latency · bitrate on one line) → Normal → Detailed. A three-finger tap on the touchscreen cycles it mid-stream."
        options={STATS_TIERS}
        value={statsTier}
        // Both keys, in sync — the same pairing `Settings::set_stats_verbosity` keeps, so a
        // client too old for the tiers still honours an Off chosen here.
        onChange={(v) => patch({ stats_verbosity: v, show_stats: v !== "off" })}
      />
      <ToggleField
        label="Wake hosts automatically"
        description="Send Wake-on-LAN to a sleeping host before connecting and wait for it to boot. Turn it off for hosts reached over a VPN, where an offline-looking host is really just unreachable by broadcast and the wait only adds delay."
        checked={s.auto_wake ?? true}
        onChange={(v) => patch({ auto_wake: v })}
      />
      <ToggleField
        label="Show game library in the client"
        description="Lets the client's own host cards browse a paired host's games. This plugin's library browser works either way — this is for the client's screens."
        checked={s.library_enabled ?? false}
        onChange={(v) => patch({ library_enabled: v })}
      />
      <ToggleField
        label="Start streams fullscreen"
        description="Streams open fullscreen instead of windowed. Launches from this plugin are always fullscreen whatever this says — it's here because the desktop client reads the same settings."
        checked={s.fullscreen_on_stream ?? true}
        onChange={(v) => patch({ fullscreen_on_stream: v })}
      />
    </div>
  );
};

// ----------------------------------------------------------------------------------------

export const SettingsSection: FC = () => {
  const [s, setS] = useState<StreamSettings | null>(null);
  // null until the enumeration answers — the pickers show a loading state rather than briefly
  // claiming this device has no endpoints.
  const [devices, setDevices] = useState<DeviceLists | null>(null);
  const [reading, setReading] = useState(true);

  const readDevices = (again: boolean) => {
    setReading(true);
    void (again ? refreshDevices() : listDevices())
      .then(setDevices)
      .finally(() => setReading(false));
  };

  useEffect(() => {
    void getSettings().then(setS);
    // Deliberately not awaited together with the settings: a cold flatpak initialising Vulkan
    // takes seconds, and the rest of the screen must not wait for it.
    readDevices(false);
  }, []);

  const patch = (p: Partial<StreamSettings>) => {
    setS((cur) => {
      if (!cur) return cur;
      const next = { ...cur, ...p };
      void setSettings(next);
      return next;
    });
  };

  if (!s) return <Spinner style={{ height: "1.5em" }} />;

  const ctx: PageCtx = { s, patch, devices, reading, readDevices };
  return (
    <SidebarNavigation
      // We are already inside the plugin's own `/punktfunk` route, rendered in a tab. Route
      // reporting would have this nav push entries of its own onto the router and fight the
      // page for the back gesture; the pages are addressed by `identifier` instead.
      disableRouteReporting
      pages={[
        { title: "Stream", identifier: "stream", icon: <FaDesktop />, content: <StreamPage {...ctx} /> },
        { title: "Video", identifier: "video", icon: <FaVideo />, content: <VideoPage {...ctx} /> },
        {
          title: "Presentation",
          identifier: "presentation",
          icon: <FaTv />,
          content: <PresentationPage {...ctx} />,
        },
        { title: "Audio", identifier: "audio", icon: <FaVolumeUp />, content: <AudioPage {...ctx} /> },
        {
          title: "Controllers",
          identifier: "controllers",
          icon: <FaGamepad />,
          content: <ControllersPage {...ctx} />,
        },
        {
          title: "Touch & mouse",
          identifier: "pointer",
          icon: <FaHandPointer />,
          content: <PointerPage {...ctx} />,
        },
        {
          title: "Interface",
          identifier: "interface",
          icon: <FaSlidersH />,
          content: <InterfacePage {...ctx} />,
        },
      ]}
    />
  );
};
