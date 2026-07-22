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
import { definePlugin, routerHook, toaster } from "@decky/api";
import { FC } from "react";
import {
  FaDownload,
  FaLock,
  FaLockOpen,
  FaPlay,
  FaPlus,
  FaSyncAlt,
  FaTv,
} from "react-icons/fa";
import { PluginErrorBoundary } from "./boundary";
import {
  applyUpdate,
  checkForUpdatesNow,
  hasUpdate,
  mergeHosts,
  needsPair,
  pinIsOnline,
  startStream,
  toHost,
  useHosts,
  usePins,
  useSavedHosts,
  useUpdate,
} from "./hooks";
import { streamPin } from "./library";
import { PunktfunkRoute, ROUTE } from "./page";
import { PairModal } from "./pair";
import { ensureGamepadUiShortcut, recreateShortcuts } from "./steam";

// Recovery action for "the Punktfunk library entry vanished" — recreates the visible shortcut.
// Deleting the shortcut (optionally + reinstalling the plugin) leaves a stale appId in Steam's
// CEF localStorage that self-heal fixes on the next mount, but this gives an in-session button
// that works even without a reload. Always ends in a toast so the tap has feedback.
async function recreatePunktfunkShortcut(): Promise<void> {
  const appId = await recreateShortcuts();
  toaster.toast({
    title: "Punktfunk",
    body: appId != null ? "Shortcut restored to your library" : "Couldn't create the shortcut",
  });
}

// ----------------------------------------------------------------------------------------
// QAM panel — quick status + entry into the full page + one-tap stream for known hosts
// and pinned games.
// ----------------------------------------------------------------------------------------
const QamPanel: FC = () => {
  const { hosts: discovered, scanning, refresh: refreshDiscovered } = useHosts();
  const { saved, loading: loadingSaved, refresh: refreshSaved } = useSavedHosts();
  const { info: update, checking, check } = useUpdate();
  const pins = usePins();

  const hosts = mergeHosts(saved, discovered);
  const busy = scanning || loadingSaved;
  const refresh = () => {
    void refreshDiscovered();
    void refreshSaved();
  };

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
        <PanelSection title="Pinned Games">
          {pins.pins.map((pin) => {
            const online = pinIsOnline(pin, hosts);
            return (
              <PanelSectionRow key={`${pin.host_fp}:${pin.game_id}`}>
                <ButtonItem
                  layout="below"
                  onClick={() => streamPin(pin, hosts.map(toHost), pins)}
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
          <ButtonItem layout="below" onClick={refresh} disabled={busy}>
            {busy ? (
              <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
            ) : (
              <FaSyncAlt style={{ marginRight: "0.5em" }} />
            )}
            {busy ? "Scanning…" : "Refresh"}
          </ButtonItem>
        </PanelSectionRow>
        {hosts.length === 0 && busy && (
          <PanelSectionRow>
            <Field focusable={false} description="Scanning your network…" />
          </PanelSectionRow>
        )}
        {hosts.length === 0 && !busy && (
          <PanelSectionRow>
            <Field
              focusable={false}
              label="No hosts found"
              description="Open Punktfunk to add a host by address, or start a host on this network and refresh."
            />
          </PanelSectionRow>
        )}
        {hosts.map((v) => {
          const pair = needsPair(v);
          const h = toHost(v);
          return (
            <PanelSectionRow key={v.fp || `${v.addr}:${v.port}`}>
              <ButtonItem
                layout="below"
                onClick={() =>
                  pair
                    ? showModal(<PairModal host={h} onPaired={() => startStream(h)} />)
                    : startStream(h)
                }
                label={
                  <span style={{ display: "inline-flex", alignItems: "center", gap: "0.4em" }}>
                    {pair ? <FaLock /> : <FaLockOpen />}
                    {v.name}
                  </span>
                }
                description={`${v.addr}:${v.port} · ${v.online ? "online" : "offline"}${
                  pair ? " · pairing required" : v.paired ? " · paired" : ""
                }`}
              >
                {pair ? "Pair & Stream" : "Stream"}
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
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            description="Missing the Punktfunk entry in your library? This puts it back."
            onClick={() => void recreatePunktfunkShortcut()}
          >
            <FaPlus style={{ marginRight: "0.5em" }} />
            Recreate library shortcut
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>
    </>
  );
};

export default definePlugin(() => {
  routerHook.addRoute(ROUTE, PunktfunkRoute, { exact: true });
  // Ensure the visible, stateless "Punktfunk" library entry (opens the gamepad UI / console
  // home) exists and is repointed to the current plugin dir — also installs the native-touch
  // controller config. Fire-and-forget: cosmetic library upkeep must never block plugin load.
  void ensureGamepadUiShortcut();
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
