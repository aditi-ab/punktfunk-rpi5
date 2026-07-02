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
} from "react-icons/fa";
import { Host, UpdateInfo, killStream } from "./backend";
import { PluginErrorBoundary } from "./boundary";
import {
  DOCS_URL,
  applyUpdate,
  checkForUpdatesNow,
  startStream,
  useHosts,
  useUpdate,
} from "./hooks";
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
const HostRow: FC<{ host: Host; onPaired: () => void }> = ({ host, onPaired }) => {
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
        <DialogButton
          style={{ width: "3em", minWidth: "3em", padding: 0 }}
          onClick={() => showModal(<HostDetailsModal host={host} />)}
        >
          <FaInfoCircle />
        </DialogButton>
        {needsPair && (
          <DialogButton
            style={{ minWidth: "5em" }}
            onClick={() => showModal(<PairModal host={host} onPaired={onPaired} />)}
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
        label="No hosts found"
        description="Start a Punktfunk host on the same network, then refresh. The setup guide (About tab) covers installing a host."
      />
    )}
    {hosts.map((h) => (
      <HostRow key={h.fp || `${h.host}:${h.port}`} host={h} onPaired={refresh} />
    ))}
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
      <DialogButton
        style={{ minWidth: "11em" }}
        disabled={checking}
        onClick={() => void checkForUpdatesNow(check)}
      >
        {checking ? <Spinner style={{ height: "1em" }} /> : "Check for updates"}
      </DialogButton>
    </Field>
    {update?.update_available && (
      <Field
        label={`Update available — v${update.latest}`}
        description="Installing can take a couple of minutes; Decky reloads the plugin when done"
        childrenContainerWidth="max"
      >
        <DialogButton style={{ minWidth: "9em" }} onClick={() => applyUpdate(update)}>
          <FaDownload style={{ marginRight: "0.4em" }} />
          Update
        </DialogButton>
      </Field>
    )}
    <Field
      label="Setup guide"
      description="Hosts, pairing, controllers, and troubleshooting — docs.punktfunk.unom.io"
      childrenContainerWidth="max"
    >
      <DialogButton
        style={{ minWidth: "8em" }}
        onClick={() => Navigation.NavigateToExternalWeb(DOCS_URL)}
      >
        <FaExternalLinkAlt style={{ marginRight: "0.4em" }} />
        Open
      </DialogButton>
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
      <DialogButton style={{ minWidth: "8em" }} onClick={() => void forceStopStream()}>
        Force-stop
      </DialogButton>
    </Field>
  </div>
);

const PunktfunkPage: FC = () => {
  const { hosts, scanning, refresh } = useHosts();
  const { info: update, checking, check } = useUpdate();
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
          Punktfunk
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
