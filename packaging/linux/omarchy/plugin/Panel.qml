import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// One bar icon and one panel, the shape every first-party rich widget uses
// (`plugins/panels/*/Panel.qml`): a `Panel` root owning the open/close lifecycle, a
// `BarIconButton` for the bar, a `KeyboardPanel` for the popup, and a local service child.
//
// The daily 95 %: what is streaming, who is asking to pair, which devices are trusted, and how the
// virtual displays behave. The web console stays the deep surface — the full access matrix,
// settings, logs and the game library are one click away and deliberately not duplicated here.
//
// **Why tabs.** The sections stacked in one column were taller than the popup could show, so the
// ones at the bottom were reachable only by growing the panel past the screen. Tabs make each
// subject's height independent.
Panel {
  id: root
  moduleName: "punktfunk"
  // Summonable from outside (`qs ipc call punktfunk toggle`) like every first-party panel — that
  // is what a Hypr keybind or a menu row can call. A bar-widget instantiates once per monitor, so
  // on a multi-head box the extra instances log the same "registered but will not be used"
  // warning the first-party panels do; the first registration wins and that is fine.
  manageIpc: true
  ipcTarget: "punktfunk"

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  // Devices waiting for approval, or a Moonlight client waiting on its PIN: the two states where
  // nothing happens until a person acts.
  readonly property bool needsYou: service.pending > 0 || service.pinPending

  readonly property color glyphColor: {
    if (service.pinMismatch || needsYou) return urgent
    return service.state === "stopped" ? Qt.darker(barForeground, 1.55) : barForeground
  }

  // The selected tab. Sticky across opens — an operator who was reading Stats wants Stats again —
  // except when something is waiting on them, which `onOpenedChanged` overrides below.
  property string tab: "now"

  function tail(fp) {
    var s = String(fp || "")
    return s.length > 10 ? "…" + s.slice(-10) : (s || "—")
  }

  // kbps is what the encoder publishes; Mbps is what an operator reads. One decimal: the adaptive
  // bitrate moves in steps far larger than 0.1 Mbps, so more digits would only ever be noise.
  function mbps(kbps) { return (Number(kbps || 0) / 1000).toFixed(1) }

  Service {
    id: service
    toasts: root.setting("toasts", true)
  }

  // Re-snapshot whenever the panel is opened: the widget's incremental state is good enough for an
  // icon, but a list somebody is about to act on should be fresh. A pending device or a waiting PIN
  // is why the panel was opened at all, so it also picks the tab.
  onOpenedChanged: {
    if (!opened) return
    if (root.needsYou) root.tab = "pairing"
    service.refresh()
    service.refreshClients()
    root.syncTab()
  }

  // A tab pays for its own data on arrival rather than every tab paying at open. `ctl` is one
  // process per call, so refreshing all of them would fan out spawns for the one pane on screen.
  function syncTab() {
    if (!opened) return
    if (root.tab === "displays") service.refreshDisplays()
  }

  onTabChanged: root.syncTab()

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    iconComponent: Component {
      Item {
        // The punktfunk lens mark, monochrome so it sits in the bar like a first-party widget:
        // identity through SHAPE (the two rings), state through COLOR (the same glyphColor the
        // old glyphs used) — plus one live cue, the lens between the rings fills while streaming.
        // Geometry is the brand mark's own: equal circles, centers a diagonal ≈ 1.46 r apart.
        Canvas {
          id: mark
          anchors.centerIn: parent
          width: Style.space(11)
          height: Style.space(11)
          visible: !service.pinMismatch
          onPaint: {
            var ctx = getContext("2d"); ctx.reset()
            var u = width / 100
            ctx.strokeStyle = String(root.glyphColor)
            ctx.fillStyle = String(root.glyphColor)
            ctx.lineWidth = 10 * u
            ctx.beginPath(); ctx.arc(34 * u, 66 * u, 29 * u, 0, 2 * Math.PI); ctx.stroke()
            ctx.beginPath(); ctx.arc(66 * u, 34 * u, 29 * u, 0, 2 * Math.PI); ctx.stroke()
            if (service.state === "streaming") {
              ctx.save()
              ctx.beginPath(); ctx.arc(34 * u, 66 * u, 29 * u, 0, 2 * Math.PI); ctx.clip()
              ctx.beginPath(); ctx.arc(66 * u, 34 * u, 29 * u, 0, 2 * Math.PI); ctx.fill()
              ctx.restore()
            }
          }
          Connections { target: root; function onGlyphColorChanged() { mark.requestPaint() } }
          Connections { target: service; function onStateChanged() { mark.requestPaint() } }
        }
        // A PIN mismatch is the one state where a generic warning reads better than the brand.
        Text {
          anchors.centerIn: parent
          visible: service.pinMismatch
          text: "󰀦"
          color: root.glyphColor
          font.family: root.fontFamily
          font.pixelSize: Style.space(11)
        }
        // The count rides the icon rather than taking a second bar slot.
        Rectangle {
          visible: root.needsYou
          anchors { right: parent.right; top: parent.top }
          width: Style.space(6); height: width; radius: width / 2
          color: root.urgent
        }
      }
    }
    onPressed: function (buttonCode) {
      // Right-click goes straight to the deep surface, matching how the other widgets treat their
      // "full settings" escape hatch.
      if (buttonCode === Qt.RightButton) service.openConsole()
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    // Wider than the old single column: the tab chips have to sit on one row.
    contentWidth: panel.fittedContentWidth(Style.space(460))
    contentHeight: panel.fittedContentHeight(column.implicitHeight, Style.space(560))

    ColumnLayout {
      id: column
      width: parent ? parent.width : 0
      spacing: Style.spacing.sm

      // ── header ─────────────────────────────────────────────────────────────────────────────
      RowLayout {
        Layout.fillWidth: true
        spacing: Style.spacing.sm

        Text {
          text: "Punktfunk"
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
          font.bold: true
        }
        Text {
          Layout.fillWidth: true
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
          text: service.state === "streaming"
                  ? service.sessions + (service.sessions === 1 ? " session" : " sessions")
                  : (service.state === "idle" ? "idle" : "not running")
        }
        PanelActionButton {
          iconText: "󰑓"
          tooltipText: service.state === "stopped" ? "Start the host" : "Restart the host"
          foreground: root.foreground
          onClicked: {
            service.detached(["systemctl", "--user",
                              service.state === "stopped" ? "start" : "restart",
                              "punktfunk-host.service"])
            settle.restart()
          }
        }
        PanelActionButton {
          iconText: "󰖟"
          tooltipText: "Open the web console"
          foreground: root.foreground
          onClicked: service.openConsole()
        }
      }

      // systemd is asynchronous and the API only answers once the host is listening, so give it a
      // moment rather than painting "not running" over a host that is still starting.
      Timer {
        id: settle
        interval: 1500
        onTriggered: { service.refresh(); service.refreshClients(); root.syncTab() }
      }

      // ── the one banner worth interrupting for ──────────────────────────────────────────────
      // Above the tabs, not inside one: it says no credential was sent, which is true of every tab.
      Text {
        visible: service.pinMismatch
        Layout.fillWidth: true
        wrapMode: Text.Wrap
        color: root.urgent
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
        text: "The process answering the management port did not present this host's certificate, "
            + "so no credential was sent. Either the host regenerated its identity, or something "
            + "else is on that port."
      }

      // ── tabs ───────────────────────────────────────────────────────────────────────────────
      // `ButtonGroup` from qs.Ui rather than a hand-rolled chip row: it is the shell's own
      // pick-one-of-N control, so the chips inherit the same selected / hover / focus painting as
      // every other Omarchy surface and keyboard walking (h/l + Enter) comes with it.
      ButtonGroup {
        Layout.fillWidth: true
        options: [
          { value: "now", label: "Now" },
          // The count rides the label so the badge on the bar icon has somewhere to land.
          { value: "pairing",
            label: service.pending > 0 ? "Pairing · " + service.pending : "Pairing" },
          { value: "devices", label: "Devices" },
          { value: "displays", label: "Displays" }
        ]
        value: root.tab
        // Bar-widget panels drive their own cursor and never hand Tab focus to a ButtonGroup (the
        // component's own docs say so); leaving it focusable would put a second focus owner inside
        // a popup that already has one.
        focusable: false
        foreground: root.foreground
        accent: root.urgent
        fontFamily: root.fontFamily
        fontSize: Style.font.caption
        onChanged: function (v) { root.tab = v }
      }

      PanelSeparator { Layout.fillWidth: true }

      // ── NOW ────────────────────────────────────────────────────────────────────────────────
      ColumnLayout {
        id: nowTab
        Layout.fillWidth: true
        spacing: Style.spacing.sm
        visible: root.tab === "now"

        readonly property bool active: service.sessions > 0 || service.games.length > 0

        Text {
          visible: !nowTab.active
          Layout.fillWidth: true
          wrapMode: Text.Wrap
          text: service.state === "stopped"
                  ? "The host is not running."
                  : "Nothing is streaming. Devices you have paired can connect at any time."
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        Text {
          visible: nowTab.active && service.games.length === 0
          Layout.fillWidth: true
          text: "Streaming the desktop."
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        Repeater {
          model: service.games
          RowLayout {
            Layout.fillWidth: true
            spacing: Style.spacing.sm
            ColumnLayout {
              Layout.fillWidth: true
              spacing: 0
              Text {
                Layout.fillWidth: true
                text: modelData.title || "Desktop"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
                elide: Text.ElideRight
              }
              Text {
                Layout.fillWidth: true
                text: (modelData.client || "—") + " · " + (modelData.plane || "")
                    + (modelData.state === "grace" ? " · reconnecting" : "")
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
                elide: Text.ElideRight
              }
            }
          }
        }

        RowLayout {
          visible: nowTab.active
          Layout.fillWidth: true
          spacing: Style.spacing.sm
          Item { Layout.fillWidth: true }
          PanelActionButton {
            iconText: "󰗼"
            tooltipText: "End the game"
            foreground: root.foreground
            onClicked: service.run(["end-game"], function () { service.refresh() })
          }
          PanelActionButton {
            iconText: "󰚌"
            tooltipText: "Stop the session"
            foreground: root.foreground
            onClicked: service.run(["stop-session"], function () { service.refresh() })
          }
        }
      }

      // ── PAIRING ────────────────────────────────────────────────────────────────────────────
      ColumnLayout {
        id: pairingTab
        Layout.fillWidth: true
        spacing: Style.spacing.sm
        visible: root.tab === "pairing"

        RowLayout {
          Layout.fillWidth: true
          spacing: Style.spacing.sm
          Text {
            Layout.fillWidth: true
            wrapMode: Text.Wrap
            color: root.foreground
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            text: service.armed
                    ? (service.pairingPin
                         ? "Pairing is open — enter " + service.pairingPin + " on the device"
                         : "Pairing is open")
                    : "Open a pairing window, then add this host on the device."
          }
          PanelActionButton {
            iconText: service.armed ? "󰅖" : "󰐕"
            tooltipText: service.armed ? "Close the pairing window" : "Open a pairing window"
            foreground: root.foreground
            onClicked: service.run(service.armed ? ["pair", "disarm"] : ["pair", "arm"],
                                   function () { service.refresh() })
          }
        }

        // The Moonlight/GameStream flow runs the other way round: the CLIENT shows a PIN and the
        // host is what needs telling. This is the field an Omarchy user expects to find here.
        RowLayout {
          visible: service.pinPending
          Layout.fillWidth: true
          spacing: Style.spacing.sm
          TextField {
            id: pinField
            Layout.preferredWidth: Style.space(90)
            placeholderText: "Moonlight PIN"
          }
          PanelActionButton {
            iconText: "󰄬"
            tooltipText: "Submit the PIN"
            foreground: root.foreground
            onClicked: if (pinField.text.length > 0)
              service.run(["pin", pinField.text], function () { pinField.text = ""; service.refresh() })
          }
        }

        Repeater {
          model: service.pendingDevices
          RowLayout {
            Layout.fillWidth: true
            spacing: Style.spacing.sm
            ColumnLayout {
              Layout.fillWidth: true
              spacing: 0
              Text {
                Layout.fillWidth: true
                text: modelData.name || "(unnamed device)"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
                elide: Text.ElideRight
              }
              Text {
                Layout.fillWidth: true
                text: root.tail(modelData.fingerprint) + " · " + (modelData.age_secs || 0) + "s ago"
                color: root.dim
                font.family: "monospace"
                font.pixelSize: Style.font.caption
              }
            }
            PanelActionButton {
              iconText: "󰅖"
              tooltipText: "Deny"
              foreground: root.urgent
              onClicked: service.run(["deny", String(modelData.id)], function () { service.refresh() })
            }
            PanelActionButton {
              iconText: "󰄬"
              tooltipText: "Approve"
              foreground: root.foreground
              // By id, never "the newest": two devices knocking at once is exactly when a
              // "newest" shortcut admits the wrong one.
              onClicked: service.run(["approve", String(modelData.id)], function () {
                service.refresh(); service.refreshClients()
              })
            }
          }
        }
      }

      // ── DEVICES ────────────────────────────────────────────────────────────────────────────
      ColumnLayout {
        id: devicesTab
        Layout.fillWidth: true
        spacing: Style.spacing.sm
        visible: root.tab === "devices"

        readonly property int deviceCount:
          service.nativeClients.length + service.gamestreamClients.length

        PanelSectionHeader {
          visible: devicesTab.deviceCount > 0
          text: "Paired · " + devicesTab.deviceCount
          foreground: root.dim; fontFamily: root.fontFamily
        }

        Text {
          visible: devicesTab.deviceCount === 0
          Layout.fillWidth: true
          wrapMode: Text.Wrap
          text: "No devices are paired yet. Open a pairing window under Pairing, then add this "
              + "host on the device."
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        Repeater {
          model: service.nativeClients.concat(service.gamestreamClients)
          Item {
            id: deviceRow
            Layout.fillWidth: true
            implicitHeight: deviceCols.implicitHeight
            // A GameStream row has a `label` and no grants; a native one has `name` and an access
            // level. Telling them apart matters: `access` is native-only, and offering it on a
            // Moonlight device would be an action that cannot work.
            readonly property bool isNative: modelData.access_level !== undefined
                                          || modelData.name !== undefined
            // Unpair is rare and destructive, and a standing column of red trash cans was the
            // loudest thing in the panel. The button shows while the pointer is on the row — the
            // bluetooth panel's forget-button idiom. A HoverHandler, not a MouseArea: hover
            // handlers stay hovered over child items that take their own hover, so the button
            // does not vanish under the pointer that is about to press it.
            HoverHandler { id: rowHover }
            RowLayout {
              id: deviceCols
              anchors.left: parent.left
              anchors.right: parent.right
              spacing: Style.spacing.sm
              ColumnLayout {
                Layout.fillWidth: true
                spacing: 0
                Text {
                  Layout.fillWidth: true
                  text: modelData.name || modelData.label || "(unnamed device)"
                  color: root.foreground
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  elide: Text.ElideRight
                }
                Text {
                  Layout.fillWidth: true
                  text: root.tail(modelData.fingerprint)
                      + " · " + (deviceRow.isNative ? "punktfunk" : "moonlight")
                      + (modelData.access_level ? " · " + modelData.access_level : "")
                  color: root.dim
                  font.family: "monospace"
                  font.pixelSize: Style.font.caption
                  elide: Text.ElideRight
                }
              }
              PanelActionButton {
                visible: rowHover.hovered
                iconText: "󰗨"
                tooltipText: "Unpair this device"
                foreground: root.urgent
                onClicked: service.run(["unpair", modelData.fingerprint], function () {
                  service.refreshClients(); service.refresh()
                })
              }
            }
          }
        }

        Text {
          visible: devicesTab.deviceCount > 0
          Layout.fillWidth: true
          wrapMode: Text.Wrap
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
          text: "Renaming and the full access matrix live in the console."
        }
      }

      // ── DISPLAYS ───────────────────────────────────────────────────────────────────────────
      // The preset picker and the policy it puts in force. NOT a list of live virtual displays:
      // on wlroots the capture arrives over a sandboxed xdp portal fd, which the host cannot
      // re-open per attach, so `vdisplay::registry` passes those displays through instead of
      // owning them (its own module docs say so) and `/display/state` is structurally empty on
      // every Omarchy box. Measured on glass: a live 2414x1188@240 head, `displays: []`. A "live
      // displays" list here would be a permanent "none" beside a screen you can point at.
      ColumnLayout {
        id: displaysTab
        Layout.fillWidth: true
        spacing: Style.spacing.sm
        visible: root.tab === "displays"

        // Built-ins and the operator's saved presets share one id space (that is what lets
        // `ctl display preset <id>` take either), so they render as one list. `name` is the saved
        // ones' label; `summary` is the built-ins'.
        readonly property var allPresets: service.displayPresets.concat(service.customPresets)

        Text {
          visible: displaysTab.allPresets.length === 0
          Layout.fillWidth: true
          wrapMode: Text.Wrap
          text: service.state === "stopped"
                  ? "The host is not running."
                  : "This host reports no display presets."
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        Repeater {
          model: displaysTab.allPresets
          Item {
            id: presetRow
            Layout.fillWidth: true
            implicitHeight: presetCols.implicitHeight
            // A saved preset is stored as a `custom` policy carrying its fields, so the stored
            // preset id can never equal a saved one's — only the built-ins can be ticked. Ticking
            // nothing beats ticking the wrong row; the "In force" line below covers that case.
            readonly property bool current: service.displayPreset === modelData.id
            HoverHandler { id: presetHover }
            TapHandler { onTapped: service.setDisplayPreset(modelData.id) }
            RowLayout {
              id: presetCols
              anchors.left: parent.left
              anchors.right: parent.right
              spacing: Style.spacing.sm
              Text {
                text: presetRow.current ? "󰄬" : " "
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
              }
              ColumnLayout {
                Layout.fillWidth: true
                spacing: 0
                Text {
                  Layout.fillWidth: true
                  text: modelData.name || modelData.id
                  color: presetRow.current || presetHover.hovered ? root.foreground : root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  elide: Text.ElideRight
                }
                Text {
                  Layout.fillWidth: true
                  visible: text.length > 0
                  text: modelData.summary || (modelData.fields ? "Saved preset." : "")
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption
                  wrapMode: Text.Wrap
                }
              }
            }
          }
        }

        PanelSeparator { Layout.fillWidth: true; visible: displaysTab.allPresets.length > 0 }

        // What is actually in force, spelled out. The stored policy reads `custom` whenever a
        // saved preset or a hand-edited console policy is the one ruling, and then no row above is
        // ticked — this line is the only thing that answers "so what is it doing right now?".
        Text {
          visible: displaysTab.allPresets.length > 0 && text.length > 0
          Layout.fillWidth: true
          wrapMode: Text.Wrap
          text: {
            var e = service.displayEffective || {}
            if (!e.topology) return ""
            return "In force: " + e.topology + " topology · " + e.identity + " identity · "
                 + e.mode_conflict + " on a mode clash · up to " + e.max_displays + " displays."
          }
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        // Said once, here, because several preset summaries promise it and this compositor cannot
        // deliver it: a wlroots portal capture cannot be re-opened per attach, so the display goes
        // with the session whatever the preset's lifecycle says.
        Text {
          visible: displaysTab.allPresets.length > 0
          Layout.fillWidth: true
          wrapMode: Text.Wrap
          text: "A display that outlives a disconnect needs a compositor whose capture survives "
              + "one. Under Hyprland the capture is a portal handle, so the display is torn down "
              + "with the session whichever preset is picked."
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }
      }

    }
  }
}
