import {
  ButtonItem,
  Dropdown,
  Field,
  Focusable,
  DialogButton,
  ModalRoot,
  Navigation,
  PanelSection,
  PanelSectionRow,
  SliderField,
  Spinner,
  ToggleField,
  showModal,
  staticClasses,
} from "@decky/ui";
import { definePlugin, routerHook, toaster } from "@decky/api";
import { FC, useCallback, useEffect, useState } from "react";
import {
  FaTv,
  FaSyncAlt,
  FaLock,
  FaLockOpen,
  FaPlay,
  FaArrowLeft,
} from "react-icons/fa";
import {
  discover,
  getSettings,
  pair,
  setSettings,
  Host,
  StreamSettings,
} from "./backend";
import { launchStream } from "./steam";

const ROUTE = "/punktfunk";

// ----------------------------------------------------------------------------------------
// Discovery hook — shared by the QAM panel and the full page.
// ----------------------------------------------------------------------------------------
function useHosts() {
  const [hosts, setHosts] = useState<Host[]>([]);
  const [scanning, setScanning] = useState(false);

  const refresh = useCallback(async () => {
    setScanning(true);
    try {
      setHosts(await discover());
    } catch (e) {
      toaster.toast({ title: "punktfunk", body: `Discovery failed: ${e}` });
    } finally {
      setScanning(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { hosts, scanning, refresh };
}

async function startStream(h: Host) {
  try {
    await launchStream(h.host, h.port);
    Navigation.CloseSideMenus();
    toaster.toast({ title: "punktfunk", body: `Starting stream — ${h.name}` });
  } catch (e) {
    toaster.toast({ title: "punktfunk", body: `Launch failed: ${e}` });
  }
}

// ----------------------------------------------------------------------------------------
// PIN pairing modal — a gamepad-navigable digit grid (the OSK is unreliable in Gaming Mode).
// The host displays the PIN after the operator arms pairing; the user enters it here.
// ----------------------------------------------------------------------------------------
const PairModal: FC<{
  host: Host;
  closeModal?: () => void;
  onPaired: () => void;
}> = ({ host, closeModal, onPaired }) => {
  const [pin, setPin] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const press = (d: string) => setPin((p) => (p.length >= 4 ? p : p + d));
  const back = () => setPin((p) => p.slice(0, -1));

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const res = await pair(host.host, host.port, pin, "Steam Deck");
      if (res.ok) {
        toaster.toast({ title: "punktfunk", body: `Paired with ${host.name}` });
        onPaired();
        closeModal?.();
      } else {
        setError(res.error ?? "pairing failed");
        setPin("");
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalRoot closeModal={closeModal}>
      <div style={{ fontWeight: "bold", fontSize: "1.3em", marginBottom: "0.3em" }}>
        Pair with {host.name}
      </div>
      <div style={{ opacity: 0.8, marginBottom: "1em" }}>
        Arm pairing on the host (its console or web UI), then enter the 4-digit PIN it shows.
      </div>

      <div
        style={{
          fontSize: "2.2em",
          letterSpacing: "0.4em",
          textAlign: "center",
          fontFamily: "monospace",
          minHeight: "1.4em",
          marginBottom: "0.6em",
        }}
      >
        {pin.padEnd(4, "•")}
      </div>
      {error && (
        <div style={{ color: "#ff6b6b", textAlign: "center", marginBottom: "0.6em" }}>
          {error}
        </div>
      )}

      <Focusable
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(3, 1fr)",
          gap: "0.5em",
        }}
      >
        {["1", "2", "3", "4", "5", "6", "7", "8", "9"].map((d) => (
          <DialogButton key={d} disabled={busy} onClick={() => press(d)}>
            {d}
          </DialogButton>
        ))}
        <DialogButton disabled={busy} onClick={back}>
          ⌫
        </DialogButton>
        <DialogButton disabled={busy} onClick={() => press("0")}>
          0
        </DialogButton>
        <DialogButton
          disabled={busy || pin.length !== 4}
          onClick={submit}
        >
          {busy ? <Spinner style={{ height: "1em" }} /> : "Pair"}
        </DialogButton>
      </Focusable>
    </ModalRoot>
  );
};

// ----------------------------------------------------------------------------------------
// Settings section — resolution / refresh / bitrate / gamepad, written to the client's JSON.
// ----------------------------------------------------------------------------------------
const RESOLUTIONS: [number, number, string][] = [
  [0, 0, "Native display"],
  [1280, 720, "1280 × 720"],
  [1920, 1080, "1920 × 1080"],
  [2560, 1440, "2560 × 1440"],
];
const REFRESH = [0, 30, 60, 90, 120];
const GAMEPADS = ["auto", "xbox360", "dualsense"];

const SettingsSection: FC = () => {
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
        <Dropdown
          rgOptions={RESOLUTIONS.map(([, , label], i) => ({ data: i, label }))}
          selectedOption={resIdx}
          onChange={(o) => {
            const [w, h] = RESOLUTIONS[o.data as number];
            patch({ width: w, height: h });
          }}
        />
      </Field>
      <Field label="Refresh rate" childrenContainerWidth="max">
        <Dropdown
          rgOptions={REFRESH.map((r) => ({ data: r, label: r === 0 ? "Native" : `${r} Hz` }))}
          selectedOption={s.refresh_hz}
          onChange={(o) => patch({ refresh_hz: o.data as number })}
        />
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
      <Field label="Gamepad type" childrenContainerWidth="max">
        <Dropdown
          rgOptions={GAMEPADS.map((g) => ({
            data: g,
            label: g === "auto" ? "Automatic" : g === "xbox360" ? "Xbox 360" : "DualSense",
          }))}
          selectedOption={s.gamepad}
          onChange={(o) => patch({ gamepad: o.data as string })}
        />
      </Field>
      <ToggleField
        label="Stream microphone"
        checked={s.mic_enabled}
        onChange={(v) => patch({ mic_enabled: v })}
      />
    </>
  );
};

// ----------------------------------------------------------------------------------------
// One host row on the full page.
// ----------------------------------------------------------------------------------------
const HostRow: FC<{ host: Host }> = ({ host }) => {
  const pairRequired = host.pair === "required";
  return (
    <Field
      label={
        <span style={{ display: "inline-flex", alignItems: "center", gap: "0.4em" }}>
          {pairRequired ? <FaLock /> : <FaLockOpen />}
          {host.name}
        </span>
      }
      description={`${host.host}:${host.port}${pairRequired ? " · pairing required" : ""}`}
      childrenContainerWidth="max"
    >
      <Focusable style={{ display: "flex", gap: "0.5em" }}>
        {pairRequired && (
          <DialogButton
            style={{ minWidth: "5em" }}
            onClick={() =>
              showModal(<PairModal host={host} onPaired={() => {}} />)
            }
          >
            Pair
          </DialogButton>
        )}
        <DialogButton style={{ minWidth: "6em" }} onClick={() => startStream(host)}>
          <FaPlay style={{ marginRight: "0.4em" }} />
          Stream
        </DialogButton>
      </Focusable>
    </Field>
  );
};

// ----------------------------------------------------------------------------------------
// The fullscreen page (registered as the /punktfunk route).
// ----------------------------------------------------------------------------------------
const PunktfunkPage: FC = () => {
  const { hosts, scanning, refresh } = useHosts();

  return (
    <div
      style={{
        marginTop: "40px",
        height: "calc(100% - 40px)",
        overflowY: "auto",
        padding: "0 2.5em 2.5em",
      }}
    >
      <Focusable style={{ display: "flex", alignItems: "center", gap: "1em", marginBottom: "1em" }}>
        <DialogButton
          style={{ width: "3em", minWidth: "3em" }}
          onClick={() => Navigation.NavigateBack()}
        >
          <FaArrowLeft />
        </DialogButton>
        <div className={staticClasses.Title} style={{ flex: 1 }}>
          punktfunk
        </div>
        <DialogButton style={{ width: "10em" }} disabled={scanning} onClick={refresh}>
          {scanning ? (
            <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
          ) : (
            <FaSyncAlt style={{ marginRight: "0.5em" }} />
          )}
          {scanning ? "Scanning…" : "Refresh"}
        </DialogButton>
      </Focusable>

      <div style={{ fontSize: "1.1em", fontWeight: "bold", margin: "0.5em 0" }}>Hosts</div>
      {hosts.length === 0 && !scanning && (
        <Field focusable={false}>No hosts discovered on the LAN.</Field>
      )}
      {hosts.map((h) => (
        <HostRow key={h.fp || `${h.host}:${h.port}`} host={h} />
      ))}

      <div style={{ fontSize: "1.1em", fontWeight: "bold", margin: "1.5em 0 0.5em" }}>
        Stream settings
      </div>
      <SettingsSection />
    </div>
  );
};

// ----------------------------------------------------------------------------------------
// QAM panel — quick status + entry into the full page + one-tap stream for known hosts.
// ----------------------------------------------------------------------------------------
const QamPanel: FC = () => {
  const { hosts, scanning, refresh } = useHosts();

  return (
    <>
      <PanelSection title="punktfunk">
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            onClick={() => {
              Navigation.Navigate(ROUTE);
              Navigation.CloseSideMenus();
            }}
          >
            <FaTv style={{ marginRight: "0.5em" }} />
            Open punktfunk
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={refresh} disabled={scanning}>
            {scanning ? (
              <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
            ) : (
              <FaSyncAlt style={{ marginRight: "0.5em" }} />
            )}
            {scanning ? "Scanning…" : "Refresh hosts"}
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>

      <PanelSection title="Hosts">
        {hosts.length === 0 && !scanning && (
          <PanelSectionRow>
            <Field focusable={false}>No hosts found.</Field>
          </PanelSectionRow>
        )}
        {hosts.map((h) => {
          const pairRequired = h.pair === "required";
          return (
            <PanelSectionRow key={h.fp || `${h.host}:${h.port}`}>
              <ButtonItem
                layout="below"
                onClick={() =>
                  pairRequired
                    ? showModal(<PairModal host={h} onPaired={() => startStream(h)} />)
                    : startStream(h)
                }
                label={
                  <span style={{ display: "inline-flex", alignItems: "center", gap: "0.4em" }}>
                    {pairRequired ? <FaLock /> : <FaLockOpen />}
                    {h.name}
                  </span>
                }
                description={`${h.host}:${h.port}`}
              >
                {pairRequired ? "Pair & Stream" : "Stream"}
              </ButtonItem>
            </PanelSectionRow>
          );
        })}
      </PanelSection>
    </>
  );
};

export default definePlugin(() => {
  routerHook.addRoute(ROUTE, PunktfunkPage, { exact: true });
  return {
    name: "punktfunk",
    titleView: <div className={staticClasses.Title}>punktfunk</div>,
    content: <QamPanel />,
    icon: <FaTv />,
    onDismount() {
      routerHook.removeRoute(ROUTE);
    },
  };
});
