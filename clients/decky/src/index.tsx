// Plugin entry: the Quick Access Menu panel + route registration. The fullscreen page lives
// in page.tsx; shared hooks/actions in hooks.ts; the Steam-shortcut launch in steam.ts.
import {
  ButtonItem,
  Field,
  Navigation,
  PanelSection,
  PanelSectionRow,
  Spinner,
  showModal,
  staticClasses,
} from "@decky/ui";
import { definePlugin, routerHook } from "@decky/api";
import { FC } from "react";
import { FaDownload, FaLock, FaLockOpen, FaPlay, FaSyncAlt, FaTv } from "react-icons/fa";
import { PluginErrorBoundary } from "./boundary";
import {
  applyUpdate,
  checkForUpdatesNow,
  hasUpdate,
  resolvePinHost,
  startStream,
  useHosts,
  usePins,
  useUpdate,
} from "./hooks";
import { streamPin } from "./library";
import { PunktfunkRoute, ROUTE } from "./page";
import { PairModal } from "./pair";

// ----------------------------------------------------------------------------------------
// QAM panel — quick status + entry into the full page + one-tap stream for known hosts
// and pinned games.
// ----------------------------------------------------------------------------------------
const QamPanel: FC = () => {
  const { hosts, scanning, refresh } = useHosts();
  const { info: update, checking, check } = useUpdate();
  const pins = usePins();

  return (
    <>
      {hasUpdate(update) && (
        <PanelSection title="Update available">
          <PanelSectionRow>
            <ButtonItem
              layout="below"
              onClick={() => applyUpdate(update!, check)}
              label={
                update!.update_available
                  ? `Plugin v${update!.current} → v${update!.latest}${
                      update!.client_update_available ? " + client" : ""
                    }`
                  : "New client version"
              }
              description="Installing can take a couple of minutes"
            >
              <FaDownload style={{ marginRight: "0.5em" }} />
              Update Punktfunk
            </ButtonItem>
          </PanelSectionRow>
        </PanelSection>
      )}

      <PanelSection title="Punktfunk">
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description="Host details, stream settings, and help"
            onClick={() => {
              Navigation.Navigate(ROUTE);
              Navigation.CloseSideMenus();
            }}
          >
            <FaTv style={{ marginRight: "0.5em" }} />
            Open Punktfunk
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>

      {/* Pinned games — the "jump straight into Playnite" rows. Pin games from a host's
          picker (fullscreen page → host row → games button). */}
      {pins.pins.length > 0 && (
        <PanelSection title="Games">
          {pins.pins.map((pin) => {
            const { online } = resolvePinHost(pin, hosts);
            return (
              <PanelSectionRow key={`${pin.host_fp}:${pin.game_id}`}>
                <ButtonItem
                  layout="below"
                  onClick={() => streamPin(pin, hosts, pins)}
                  label={pin.title}
                  description={`${pin.host_name}${online ? "" : " · offline?"}${
                    pin.paired ? "" : " · pairing required"
                  }`}
                >
                  <FaPlay style={{ marginRight: "0.5em" }} />
                  Stream
                </ButtonItem>
              </PanelSectionRow>
            );
          })}
        </PanelSection>
      )}

      <PanelSection title="Hosts">
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={refresh} disabled={scanning}>
            {scanning ? (
              <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
            ) : (
              <FaSyncAlt style={{ marginRight: "0.5em" }} />
            )}
            {scanning ? "Scanning…" : "Refresh"}
          </ButtonItem>
        </PanelSectionRow>
        {hosts.length === 0 && scanning && (
          <PanelSectionRow>
            <Field focusable={false} description="Scanning your network…" />
          </PanelSectionRow>
        )}
        {hosts.length === 0 && !scanning && (
          <PanelSectionRow>
            <Field
              focusable={false}
              label="No hosts found"
              description="Start a Punktfunk host on this network, then refresh."
            />
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

      <PanelSection title="About">
        <PanelSectionRow>
          <Field
            focusable={false}
            label="Version"
            description={
              update
                ? `v${update.current}${update.channel ? ` · ${update.channel}` : " · dev build"}`
                : "…"
            }
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            disabled={checking}
            onClick={() => void checkForUpdatesNow(check)}
          >
            {checking ? "Checking…" : "Check for updates"}
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>
    </>
  );
};

export default definePlugin(() => {
  routerHook.addRoute(ROUTE, PunktfunkRoute, { exact: true });
  return {
    // `name` is the plugin's INTERNAL id — it must stay in sync with plugin.json (the loader
    // keys plugins by it), so it stays lowercase; user-facing strings say "Punktfunk".
    name: "punktfunk",
    // `staticClasses?.Title` is guarded so a future client that drops the export can't throw
    // at plugin-load time (an error boundary only catches render-time, not load-time, errors).
    titleView: <div className={staticClasses?.Title}>Punktfunk</div>,
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
