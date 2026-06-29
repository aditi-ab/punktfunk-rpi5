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
  Tabs,
  ToggleField,
  showModal,
  staticClasses,
} from "@decky/ui";
import { definePlugin, routerHook, toaster } from "@decky/api";
import {
  Component,
  CSSProperties,
  ErrorInfo,
  FC,
  ReactNode,
  useCallback,
  useEffect,
  useState,
} from "react";
import {
  FaTv,
  FaSyncAlt,
  FaLock,
  FaLockOpen,
  FaPlay,
  FaArrowLeft,
  FaDownload,
} from "react-icons/fa";
import {
  discover,
  getSettings,
  pair,
  setSettings,
  checkUpdate,
  Host,
  StreamSettings,
  UpdateInfo,
} from "./backend";
import { launchStream } from "./steam";

const ROUTE = "/punktfunk";

// Decky Loader exposes its already-authenticated WSRouter as a global. This is NOT part of
// @decky/api (it's a loader internal), so we treat it as optional and guard every use — on a
// loader without it we fall back to manual "Install Plugin from URL". We use it to drive
// Decky's own privileged install path (the root loader does the download + SHA-256 verify +
// extract + hot-reload), which is the only way a plugin can update itself: ~/homebrew/plugins
// is root-owned, so our unprivileged backend can't swap its own files.
declare global {
  interface Window {
    DeckyBackend?: {
      callable: (route: string) => (...args: unknown[]) => Promise<unknown>;
    };
  }
}

// PluginInstallType.UPDATE in decky-loader's browser.py (INSTALL=0/REINSTALL=1/UPDATE=2/…).
const INSTALL_TYPE_UPDATE = 2;

// ----------------------------------------------------------------------------------------
// Error boundary — contains ANY render failure in our UI so a single bad render can never take
// down the whole Quick Access "Decky" section (Decky's tab-level boundary shows the generic
// "Something went wrong while displaying this content" for the entire tab when one plugin
// throws). The realistic trigger is a future Steam client update that makes a @decky/ui
// component resolve to `undefined` (React then throws "Element type is invalid"). The fallback
// is built from ONLY plain DOM elements + inline styles, so it cannot itself depend on a
// (possibly broken) Steam-internal component — it is guaranteed to render.
// ----------------------------------------------------------------------------------------
class PluginErrorBoundary extends Component<
  { children: ReactNode },
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // Surface it for diagnosis, but never rethrow — containment is the whole point.
    // eslint-disable-next-line no-console
    console.error("[punktfunk] contained UI render error:", error, info?.componentStack);
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div style={{ padding: "1em", lineHeight: 1.45 }}>
        <div style={{ fontWeight: "bold", marginBottom: "0.4em" }}>
          punktfunk couldn’t draw this view
        </div>
        <div style={{ opacity: 0.8, marginBottom: "0.6em" }}>
          The plugin hit a display error — your Steam Deck is fine. Reload punktfunk from
          Decky&apos;s plugin list, or update the plugin.
        </div>
        <div
          style={{
            opacity: 0.55,
            fontFamily: "monospace",
            fontSize: "0.8em",
            wordBreak: "break-word",
          }}
        >
          {String(error?.message ?? error)}
        </div>
      </div>
    );
  }
}

// Checks our registry for a newer build on mount (the backend caches + is non-fatal offline).
function useUpdate() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  useEffect(() => {
    void checkUpdate(false)
      .then(setInfo)
      .catch(() => {});
  }, []);
  return info;
}

async function applyUpdate(info: UpdateInfo) {
  try {
    const backend = window.DeckyBackend;
    if (backend?.callable) {
      // Fire-and-forget: the loader reinstalls + reloads THIS plugin, tearing the panel down
      // before any result could arrive — so never await it. Decky shows its own confirm prompt.
      void backend.callable("utilities/install_plugin")(
        info.artifact,
        "punktfunk",
        info.latest,
        info.hash,
        INSTALL_TYPE_UPDATE,
      );
      toaster.toast({
        title: "punktfunk",
        body: `Updating to v${info.latest}… confirm the Decky prompt.`,
      });
      return;
    }
  } catch {
    // fall through to the manual path
  }
  toaster.toast({
    title: "punktfunk",
    body: "Update from Decky → Developer → Install Plugin from URL.",
  });
}

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
const GAMEPADS = ["auto", "xbox360", "dualsense", "steamdeck"];
const GAMEPAD_LABELS: Record<string, string> = {
  auto: "Automatic",
  xbox360: "Xbox 360",
  dualsense: "DualSense",
  steamdeck: "Steam Deck",
};

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
          rgOptions={GAMEPADS.map((g) => ({ data: g, label: GAMEPAD_LABELS[g] ?? g }))}
          selectedOption={s.gamepad}
          onChange={(o) => patch({ gamepad: o.data as string })}
        />
      </Field>
      {s.gamepad === "steamdeck" && (
        <Field
          label="⚠ Disable Steam Input"
          description="Steam Deck mode forwards the paddles, both trackpads, and gyro to the host. For that, Steam Input must be OFF for punktfunk: on the game page tap ⚙ → Controller Settings → set Steam Input to Off. Otherwise Steam keeps the Deck's controls and only the sticks + buttons reach the host."
        />
      )}
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
  // The host's policy is `pair=required`, but if THIS device is already paired we don't need to
  // pair again — show it as trusted and go straight to Stream.
  const needsPair = host.pair === "required" && !host.paired;
  return (
    <Field
      label={
        <span style={{ display: "inline-flex", alignItems: "center", gap: "0.4em" }}>
          {needsPair ? <FaLock /> : <FaLockOpen />}
          {host.name}
        </span>
      }
      description={`${host.host}:${host.port}${
        needsPair ? " · pairing required" : host.paired ? " · paired" : ""
      }`}
      childrenContainerWidth="max"
    >
      <Focusable style={{ display: "flex", gap: "0.5em" }}>
        {needsPair && (
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
// The fullscreen page (registered as the /punktfunk route) — a tabbed Hosts / Settings view.
// ----------------------------------------------------------------------------------------

// Bottom inset so the last control clears Gaming Mode's footer hint bar. Routed pages render
// *under* that bar otherwise — that's why the last Stream-settings row was getting hidden. The
// value is generous on purpose (and harmless where the tab area already insets); tune to taste.
const SAFE_BOTTOM = "80px";

// Each tab is its own scroll area so long content is always reachable above the footer.
const tabScroll: CSSProperties = {
  height: "100%",
  overflowY: "auto",
  padding: "0.5em 2.5em",
  paddingBottom: SAFE_BOTTOM,
  boxSizing: "border-box",
};

const HostsTab: FC<{
  hosts: Host[];
  scanning: boolean;
  refresh: () => void;
}> = ({ hosts, scanning, refresh }) => (
  <div style={tabScroll}>
    <Field
      label="Discover"
      description={
        scanning
          ? "Scanning the LAN…"
          : `${hosts.length} host${hosts.length === 1 ? "" : "s"} on your network`
      }
      childrenContainerWidth="max"
      bottomSeparator={hosts.length ? "standard" : "none"}
    >
      <DialogButton style={{ minWidth: "8em" }} disabled={scanning} onClick={refresh}>
        {scanning ? (
          <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
        ) : (
          <FaSyncAlt style={{ marginRight: "0.5em" }} />
        )}
        {scanning ? "Scanning…" : "Refresh"}
      </DialogButton>
    </Field>

    {hosts.length === 0 && !scanning && (
      <Field
        focusable={false}
        description="No punktfunk hosts found. Make sure a host is running on the same network."
      >
        No hosts found
      </Field>
    )}
    {hosts.map((h) => (
      <HostRow key={h.fp || `${h.host}:${h.port}`} host={h} />
    ))}
  </div>
);

const SettingsTab: FC = () => (
  <div style={tabScroll}>
    <SettingsSection />
  </div>
);

const PunktfunkPage: FC = () => {
  const { hosts, scanning, refresh } = useHosts();
  const update = useUpdate();
  const [tab, setTab] = useState("hosts");

  return (
    <div
      style={{
        marginTop: "40px",
        height: "calc(100% - 40px)",
        display: "flex",
        flexDirection: "column",
      }}
    >
      <Focusable
        style={{
          display: "flex",
          alignItems: "center",
          gap: "1em",
          padding: "0 2.5em",
          marginBottom: "0.4em",
          flexShrink: 0,
        }}
      >
        <DialogButton
          style={{ width: "3em", minWidth: "3em", padding: 0 }}
          onClick={() => Navigation.NavigateBack()}
        >
          <FaArrowLeft />
        </DialogButton>
        <div className={staticClasses?.Title} style={{ flex: 1, margin: 0 }}>
          punktfunk
        </div>
        {update?.update_available && (
          <DialogButton style={{ minWidth: "9em" }} onClick={() => applyUpdate(update)}>
            <FaDownload style={{ marginRight: "0.4em" }} />
            Update v{update.latest}
          </DialogButton>
        )}
      </Focusable>

      <div style={{ flex: 1, minHeight: 0 }}>
        <Tabs
          activeTab={tab}
          onShowTab={(id: string) => setTab(id)}
          autoFocusContents
          tabs={[
            {
              id: "hosts",
              title: "Hosts",
              content: <HostsTab hosts={hosts} scanning={scanning} refresh={refresh} />,
            },
            {
              id: "settings",
              title: "Settings",
              content: <SettingsTab />,
            },
          ]}
        />
      </div>
    </div>
  );
};

// ----------------------------------------------------------------------------------------
// QAM panel — quick status + entry into the full page + one-tap stream for known hosts.
// ----------------------------------------------------------------------------------------
const QamPanel: FC = () => {
  const { hosts, scanning, refresh } = useHosts();
  const update = useUpdate();

  return (
    <>
      {update?.update_available && (
        <PanelSection title="Update">
          <PanelSectionRow>
            <ButtonItem
              layout="below"
              onClick={() => applyUpdate(update)}
              label={`v${update.current} → v${update.latest}`}
            >
              <FaDownload style={{ marginRight: "0.5em" }} />
              Update punktfunk
            </ButtonItem>
          </PanelSectionRow>
        </PanelSection>
      )}

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
          const needsPair = h.pair === "required" && !h.paired;
          return (
            <PanelSectionRow key={h.fp || `${h.host}:${h.port}`}>
              <ButtonItem
                layout="below"
                onClick={() =>
                  needsPair
                    ? showModal(<PairModal host={h} onPaired={() => startStream(h)} />)
                    : startStream(h)
                }
                label={
                  <span style={{ display: "inline-flex", alignItems: "center", gap: "0.4em" }}>
                    {needsPair ? <FaLock /> : <FaLockOpen />}
                    {h.name}
                  </span>
                }
                description={`${h.host}:${h.port}${h.paired ? " · paired" : ""}`}
              >
                {needsPair ? "Pair & Stream" : "Stream"}
              </ButtonItem>
            </PanelSectionRow>
          );
        })}
      </PanelSection>
    </>
  );
};

// Full page behind the boundary — registered as the /punktfunk route.
const PunktfunkRoute: FC = () => (
  <PluginErrorBoundary>
    <PunktfunkPage />
  </PluginErrorBoundary>
);

export default definePlugin(() => {
  routerHook.addRoute(ROUTE, PunktfunkRoute, { exact: true });
  return {
    name: "punktfunk",
    // `staticClasses?.Title` is guarded so a future client that drops the export can't throw
    // at plugin-load time (an error boundary only catches render-time, not load-time, errors).
    titleView: <div className={staticClasses?.Title}>punktfunk</div>,
    content: (
      <PluginErrorBoundary>
        <QamPanel />
      </PluginErrorBoundary>
    ),
    icon: <FaTv />,
    onDismount() {
      routerHook.removeRoute(ROUTE);
    },
  };
});
