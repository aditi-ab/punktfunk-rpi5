// The fullscreen page (registered as the /punktfunk route) — Hosts / Settings / About tabs.
import {
  DialogButton,
  Field,
  Focusable,
  ModalRoot,
  Navigation,
  Spinner,
  Tabs,
  showModal,
  staticClasses,
} from "@decky/ui";
import { RowActions, actionButton, iconButton } from "./ui";
import { toaster } from "@decky/api";
import { CSSProperties, FC, useState } from "react";
import {
  FaArrowLeft,
  FaDownload,
  FaExternalLinkAlt,
  FaInfoCircle,
  FaLock,
  FaLockOpen,
  FaPlay,
  FaSyncAlt,
  FaThLarge,
} from "react-icons/fa";
import { Host, UpdateInfo, killStream } from "./backend";
import { PluginErrorBoundary } from "./boundary";
import {
  DOCS_URL,
  PinsApi,
  applyUpdate,
  checkForUpdatesNow,
  hasUpdate,
  resolvePinHost,
  startStream,
  useHosts,
  usePins,
  useUpdate,
} from "./hooks";
import { GamePickerModal, storeLabel, streamPin } from "./library";
import { PairModal } from "./pair";
import { SettingsSection } from "./settings";
import { stopStream } from "./steam";

export const ROUTE = "/punktfunk";

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

// ----------------------------------------------------------------------------------------
// Host details — everything the mDNS advert told us, incl. the fingerprint to cross-check
// against the host's own log / web console before trusting it.
// ----------------------------------------------------------------------------------------
const HostDetailsModal: FC<{ host: Host; closeModal?: () => void }> = ({
  host,
  closeModal,
}) => {
  const fp = host.fp ? (host.fp.match(/.{1,4}/g) ?? [host.fp]).join(" ") : "not advertised";
  return (
    <ModalRoot closeModal={closeModal}>
      <div style={{ fontWeight: "bold", fontSize: "1.3em", marginBottom: "0.4em" }}>
        {host.name}
      </div>
      <Field focusable={false} label="Address">
        {host.host}:{host.port}
      </Field>
      <Field focusable={false} label="Protocol">
        {host.proto || "unknown"}
      </Field>
      <Field focusable={false} label="Pairing policy">
        {host.pair === "required" ? "PIN pairing required" : "Open (trust on first connect)"}
      </Field>
      <Field focusable={false} label="This Deck">
        {host.paired ? "Paired" : "Not paired yet"}
      </Field>
      <Field
        focusable={false}
        label="Certificate fingerprint (SHA-256)"
        description={
          <span
            style={{ fontFamily: "monospace", fontSize: "0.85em", wordBreak: "break-word" }}
          >
            {fp}
          </span>
        }
      />
    </ModalRoot>
  );
};

// ----------------------------------------------------------------------------------------
// One host row: status icon + address, details / pair / stream actions.
// ----------------------------------------------------------------------------------------
const HostRow: FC<{ host: Host; onPaired: () => void; onGames: () => void }> = ({
  host,
  onPaired,
  onGames,
}) => {
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
      <RowActions>
        <DialogButton
          style={iconButton}
          onClick={() => showModal(<HostDetailsModal host={host} />)}
        >
          <FaInfoCircle />
        </DialogButton>
        {/* Labeled, not icon-only: this is the entry to the game picker AND the on-screen
            library browser, and controller nav has no hover tooltip to explain a bare icon. */}
        <DialogButton style={actionButton} onClick={onGames}>
          <FaThLarge style={{ marginRight: "0.4em" }} />
          Games
        </DialogButton>
        {needsPair && (
          <DialogButton
            style={actionButton}
            onClick={() => showModal(<PairModal host={host} onPaired={onPaired} />)}
          >
            Pair
          </DialogButton>
        )}
        <DialogButton
          style={actionButton}
          onClick={() =>
            needsPair
              ? showModal(
                  <PairModal host={host} onPaired={() => startStream(host)} />,
                )
              : startStream(host)
          }
        >
          <FaPlay style={{ marginRight: "0.4em" }} />
          Stream
        </DialogButton>
      </RowActions>
    </Field>
  );
};

const HostsTab: FC<{
  hosts: Host[];
  scanning: boolean;
  refresh: () => void;
  pins: PinsApi;
  clientUpdatePending: boolean;
}> = ({ hosts, scanning, refresh, pins, clientUpdatePending }) => (
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
      <RowActions>
        <DialogButton style={actionButton} disabled={scanning} onClick={refresh}>
          {scanning ? (
            <Spinner style={{ height: "1em", marginRight: "0.5em" }} />
          ) : (
            <FaSyncAlt style={{ marginRight: "0.5em" }} />
          )}
          {scanning ? "Scanning…" : "Refresh"}
        </DialogButton>
      </RowActions>
    </Field>

    {hosts.length === 0 && !scanning && (
      <Field
        focusable={false}
        label="No hosts found"
        description="Start a Punktfunk host on the same network, then refresh. The setup guide (About tab) covers installing a host."
      />
    )}
    {hosts.map((h) => (
      <HostRow
        key={h.fp || `${h.host}:${h.port}`}
        host={h}
        onPaired={refresh}
        onGames={() =>
          showModal(
            <GamePickerModal host={h} pins={pins} clientUpdatePending={clientUpdatePending} />,
          )
        }
      />
    ))}

    {/* Pinned games — also the cleanup surface for pins whose host is gone from the scan. */}
    {pins.pins.length > 0 && (
      <>
        <Field
          focusable={false}
          label="Pinned games"
          description="One-tap streams — they also live in the quick-access menu"
          bottomSeparator="standard"
        />
        {pins.pins.map((pin) => {
          const { online } = resolvePinHost(pin, hosts);
          return (
            <Field
              key={`${pin.host_fp}:${pin.game_id}`}
              label={pin.title}
              description={`${storeLabel(pin.store)} · ${pin.host_name}${
                online ? "" : " · offline?"
              }${pin.paired ? "" : " · pairing required"}`}
              childrenContainerWidth="max"
            >
              <RowActions>
                <DialogButton style={actionButton} onClick={() => streamPin(pin, hosts, pins)}>
                  <FaPlay style={{ marginRight: "0.4em" }} />
                  Play
                </DialogButton>
                <DialogButton
                  style={actionButton}
                  onClick={() => pins.removePin(pin.host_fp, pin.game_id)}
                >
                  Remove
                </DialogButton>
              </RowActions>
            </Field>
          );
        })}
      </>
    )}
  </div>
);

const SettingsTab: FC = () => (
  <div style={tabScroll}>
    <SettingsSection />
  </div>
);

// ----------------------------------------------------------------------------------------
// About — plugin version + explicit update check, docs link, stream-exit help, force-stop.
// ----------------------------------------------------------------------------------------
async function forceStopStream(): Promise<void> {
  stopStream(); // ask Steam to end the "game" first (clean path)
  const res = await killStream(); // then the flatpak-level hammer for a wedged client
  toaster.toast({
    title: "Punktfunk",
    body: res.ok ? "Stream client stopped." : "Couldn’t stop the stream client.",
  });
}

const AboutTab: FC<{
  update: UpdateInfo | null;
  checking: boolean;
  check: (force: boolean) => Promise<UpdateInfo | null>;
}> = ({ update, checking, check }) => (
  <div style={tabScroll}>
    <Field
      label="Version"
      description={
        update
          ? `v${update.current}${
              update.channel ? ` · ${update.channel} channel` : " · development build"
            }`
          : "…"
      }
      childrenContainerWidth="max"
    >
      <RowActions>
        <DialogButton
          style={actionButton}
          disabled={checking}
          onClick={() => void checkForUpdatesNow(check)}
        >
          {checking ? <Spinner style={{ height: "1em" }} /> : "Check for updates"}
        </DialogButton>
      </RowActions>
    </Field>
    {hasUpdate(update) && (
      <Field
        label={
          update!.update_available
            ? `Plugin update — v${update!.latest}${
                update!.client_update_available ? " + client" : ""
              }`
            : "Client update available"
        }
        description="Installing can take a couple of minutes; Decky reloads the plugin when done"
        childrenContainerWidth="max"
      >
        <RowActions>
          <DialogButton style={actionButton} onClick={() => applyUpdate(update!, check)}>
            <FaDownload style={{ marginRight: "0.4em" }} />
            Update
          </DialogButton>
        </RowActions>
      </Field>
    )}
    <Field
      label="Setup guide"
      description="Hosts, pairing, controllers, and troubleshooting — docs.punktfunk.unom.io"
      childrenContainerWidth="max"
    >
      <RowActions>
        <DialogButton
          style={actionButton}
          onClick={() => Navigation.NavigateToExternalWeb(DOCS_URL)}
        >
          <FaExternalLinkAlt style={{ marginRight: "0.4em" }} />
          Open
        </DialogButton>
      </RowActions>
    </Field>
    <Field
      focusable={false}
      label="Leaving a stream"
      description="Hold L1 + R1 + Start + Select inside the stream, or close the “game” from the Steam overlay — either returns you to Gaming Mode."
    />
    <Field
      label="Stream stuck?"
      description="Force-stop the stream client if a session wedges"
      childrenContainerWidth="max"
    >
      <RowActions>
        <DialogButton style={actionButton} onClick={() => void forceStopStream()}>
          Force-stop
        </DialogButton>
      </RowActions>
    </Field>
  </div>
);

const PunktfunkPage: FC = () => {
  const { hosts, scanning, refresh } = useHosts();
  const { info: update, checking, check } = useUpdate();
  const pins = usePins();
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
      {/* Header is title + back only — updates live on the About tab (and the QAM banner). */}
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
        <DialogButton style={iconButton} onClick={() => Navigation.NavigateBack()}>
          <FaArrowLeft />
        </DialogButton>
        <div className={staticClasses?.Title} style={{ flex: 1, margin: 0 }}>
          Punktfunk
        </div>
      </Focusable>

      {/* Two things fight each other on an L1/R1 tab switch:
          1. Valve's Tabs slides the incoming panel in from the right with a CSS transform.
          2. `autoFocusContents` then focuses a control inside that still-offscreen panel, which
             fires scrollIntoView. Because the panel is offset by a *transform* (not by scroll
             position), scrollIntoView can't satisfy it by scrolling any one ancestor, so it walks
             up and pans the whole page — the "screen jumps right, then animates back" glitch.
          Dropping autoFocusContents removes the scrollIntoView entirely, so nothing fights the
          slide. L1/R1 still cycles tabs (that handler lives on the Tabs focus scope, active while
          focus is anywhere inside — including the tab strip); after a switch, focus stays on the
          strip and Down enters the content, which is how Steam's own tabbed pages behave.
          The overflow:hidden clip stays as defense-in-depth against any stray horizontal pan. */}
      <div style={{ flex: 1, minHeight: 0, overflow: "hidden" }}>
        <Tabs
          activeTab={tab}
          onShowTab={(id: string) => setTab(id)}
          tabs={[
            {
              id: "hosts",
              title: "Hosts",
              content: (
                <HostsTab
                  hosts={hosts}
                  scanning={scanning}
                  refresh={refresh}
                  pins={pins}
                  clientUpdatePending={!!update?.client_update_available}
                />
              ),
            },
            {
              id: "settings",
              title: "Settings",
              content: <SettingsTab />,
            },
            {
              id: "about",
              title: "About",
              content: <AboutTab update={update} checking={checking} check={check} />,
            },
          ]}
        />
      </div>
    </div>
  );
};

// Full page behind the boundary — registered as the /punktfunk route.
export const PunktfunkRoute: FC = () => (
  <PluginErrorBoundary>
    <PunktfunkPage />
  </PluginErrorBoundary>
);
