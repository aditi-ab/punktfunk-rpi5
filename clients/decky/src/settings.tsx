// Stream settings — resolution / refresh / bitrate / gamepad / compositor / mic, written to
// the flatpak client's JSON (main.py set_settings), which the client reads on launch. The
// accepted gamepad/compositor names mirror punktfunk-core's `*Pref::from_name`.
import { Dropdown, Field, SliderField, Spinner, ToggleField } from "@decky/ui";
import { CSSProperties, FC, useEffect, useState } from "react";
import { getSettings, setSettings, StreamSettings } from "./backend";
import { RowActions } from "./ui";

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

const RESOLUTIONS: [number, number, string][] = [
  [0, 0, "Native display"],
  [1280, 720, "1280 × 720"],
  [1280, 800, "1280 × 800 (Deck)"],
  [1920, 1080, "1920 × 1080"],
  [2560, 1440, "2560 × 1440"],
];
const REFRESH = [0, 30, 60, 90, 120];
const GAMEPADS = ["auto", "xbox360", "xboxone", "dualsense", "dualshock4", "steamdeck"];
const GAMEPAD_LABELS: Record<string, string> = {
  auto: "Automatic",
  xbox360: "Xbox 360",
  xboxone: "Xbox One",
  dualsense: "DualSense",
  dualshock4: "DualShock 4",
  steamdeck: "Steam Deck",
};
// Mirrors the desktop client's picker (ui_settings.rs CODECS) — a soft preference the host
// falls back from when its GPU can't encode it.
const CODECS = ["auto", "hevc", "h264", "av1"];
const CODEC_LABELS: Record<string, string> = {
  auto: "Automatic",
  hevc: "HEVC (H.265)",
  h264: "H.264 (AVC)",
  av1: "AV1",
};
const COMPOSITORS = ["auto", "kwin", "wlroots", "mutter", "gamescope"];
const COMPOSITOR_LABELS: Record<string, string> = {
  auto: "Automatic",
  kwin: "KDE Plasma (KWin)",
  wlroots: "Sway (wlroots)",
  mutter: "GNOME (Mutter)",
  gamescope: "gamescope",
};

export const SettingsSection: FC = () => {
  const [s, setS] = useState<StreamSettings | null>(null);

  useEffect(() => {
    void getSettings().then(setS);
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

  const resIdx = Math.max(
    0,
    RESOLUTIONS.findIndex(([w, h]) => w === s.width && h === s.height),
  );

  return (
    <>
      <Field
        label="Resolution"
        description="The host creates a virtual output at exactly this size"
        childrenContainerWidth="max"
      >
        <RowActions>
          <div style={selectShell}>
            <Dropdown
              rgOptions={RESOLUTIONS.map(([, , label], i) => ({ data: i, label }))}
              selectedOption={resIdx}
              onChange={(o) => {
                const [w, h] = RESOLUTIONS[o.data as number];
                patch({ width: w, height: h });
              }}
            />
          </div>
        </RowActions>
      </Field>
      <Field label="Refresh rate" childrenContainerWidth="max">
        <RowActions>
          <div style={selectShell}>
            <Dropdown
              rgOptions={REFRESH.map((r) => ({ data: r, label: r === 0 ? "Native" : `${r} Hz` }))}
              selectedOption={s.refresh_hz}
              onChange={(o) => patch({ refresh_hz: o.data as number })}
            />
          </div>
        </RowActions>
      </Field>
      <SliderField
        label="Bitrate"
        description="Mbit/s · 0 = host default"
        value={Math.round(s.bitrate_kbps / 1000)}
        min={0}
        max={150}
        step={5}
        showValue
        valueSuffix=" Mbit/s"
        onChange={(v) => patch({ bitrate_kbps: v * 1000 })}
      />
      <Field
        label="Video codec"
        description="Preferred stream codec — the host falls back when its GPU can't encode it"
        childrenContainerWidth="max"
      >
        <RowActions>
          <div style={selectShell}>
            <Dropdown
              rgOptions={CODECS.map((c) => ({ data: c, label: CODEC_LABELS[c] ?? c }))}
              selectedOption={s.codec ?? "auto"}
              onChange={(o) => patch({ codec: o.data as string })}
            />
          </div>
        </RowActions>
      </Field>
      <Field
        label="Gamepad type"
        description="Which virtual controller the host creates for your inputs"
        childrenContainerWidth="max"
      >
        <RowActions>
          <div style={selectShell}>
            <Dropdown
              rgOptions={GAMEPADS.map((g) => ({ data: g, label: GAMEPAD_LABELS[g] ?? g }))}
              selectedOption={s.gamepad}
              onChange={(o) => patch({ gamepad: o.data as string })}
            />
          </div>
        </RowActions>
      </Field>
      {(s.gamepad === "steamdeck" || s.gamepad === "auto") && (
        <Field
          label="⚠ Disable Steam Input"
          description="On a Deck, Automatic forwards the built-in controller as a Steam Deck pad — paddles, both trackpads, and gyro included. For that, Steam Input must be OFF for Punktfunk: on the game page tap ⚙ → Controller Settings → set Steam Input to Off. Otherwise Steam keeps the Deck's controls and only the sticks + buttons reach the host."
        />
      )}
      <Field
        label="Host compositor"
        description="Which compositor backend the host uses for the virtual display — Automatic suits almost every host"
        childrenContainerWidth="max"
      >
        <RowActions>
          <div style={selectShell}>
            <Dropdown
              rgOptions={COMPOSITORS.map((c) => ({ data: c, label: COMPOSITOR_LABELS[c] ?? c }))}
              selectedOption={s.compositor}
              onChange={(o) => patch({ compositor: o.data as string })}
            />
          </div>
        </RowActions>
      </Field>
      <ToggleField
        label="Stream microphone"
        description="Send the Deck's microphone to the host's virtual mic"
        checked={s.mic_enabled}
        onChange={(v) => patch({ mic_enabled: v })}
      />
    </>
  );
};
