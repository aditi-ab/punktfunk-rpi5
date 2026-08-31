import QtQuick
import Quickshell
import Quickshell.Io

// The plugin's data half: one long-lived `punktfunk-host ctl watch`, the REST snapshots it
// triggers, and the one function that spawns anything.
//
// **This is the security boundary.** A shell plugin runs unsandboxed inside omarchy-shell, and the
// management API's admin surface — the pending queue, the PIN, unpair, session control — is exactly
// what is worth protecting. So the QML holds no credential and speaks no HTTPS: every call spawns
// `punktfunk-host ctl`, which reads the operator token and the host's certificate from the 0700
// config directory in its own process and pins the certificate *before* sending anything. Reading
// `run()` below answers "can this plugin leak a secret?".
//
// Exactly ONE process runs continuously (`watcher`). The host caps concurrent event streams and the
// web console holds one of them, so a stream per surface would be the thing that exhausts the cap.
// `ctl watch` owns its own reconnect and Last-Event-ID resume; this file only reacts to the
// synthetic `ctl.resync` line by re-snapshotting, which is the only correct answer to "your
// incremental state may be stale".
Item {
  id: root

  // ── published state ──────────────────────────────────────────────────────────────────────────
  property string state: "stopped"        // stopped | idle | streaming
  property int sessions: 0
  property int pending: 0
  property bool pinPending: false
  property bool armed: false
  property string pairingPin: ""
  property var pendingDevices: []
  property var nativeClients: []
  property var gamestreamClients: []
  property var games: []

  // ── displays ─────────────────────────────────────────────────────────────────────────────────
  // The stored policy's preset id, the resolved policy it expands to, and the two preset
  // catalogues (built-in and saved presets share one id space). Deliberately NOT the live display
  // list `ctl display` also carries: on wlroots the registry passes displays through rather than
  // owning them, so that list is always empty here — see the Panel's DISPLAYS section.
  property string displayPreset: ""
  property var displayEffective: ({})
  property var displayPresets: []
  property var customPresets: []

  // ── summary ──────────────────────────────────────────────────────────────────────────────────
  // `/status` exposes no device names by design, so it cannot say WHO is streaming. This is the one
  // endpoint that names the connected client, and it carries the host version with it.
  property var summary: ({})

  // ── stats ────────────────────────────────────────────────────────────────────────────────────
  // `stream` is free and live: its `bitrate_kbps` is the encoder's current target, so it moves with
  // every adaptive-bitrate change. The rest only exists while a capture is armed, because that is
  // when the streaming loops emit samples at all.
  property var stream: null
  property var sessionMode: null
  property bool audioStreaming: false
  property bool captureArmed: false
  property int captureSamples: 0
  property var statsSample: null
  property var statsMeta: null


  // A certificate mismatch is NOT "the host is down": something that is not our host answered on
  // the management port, and ctl refused to send the token. Surfaced separately so the panel can
  // say so instead of showing a plausible-looking "not running".
  property bool pinMismatch: false
  property string lastError: ""
  property bool toasts: true

  readonly property int exitPin: 4

  // ── the one place anything is spawned ────────────────────────────────────────────────────────
  //
  // ⚠ Quickshell's `Process` does NOT search `PATH` — it reported "the binary could not be found"
  // for `punktfunk-host` on a box where /usr/bin/punktfunk-host was present, executable, and
  // /usr/bin was in the shell process's own PATH. Hardcoding /usr/bin would be wrong (a sysext or
  // a /usr/local build lives elsewhere), so the command goes through `sh -c 'exec "$@"' sh …`:
  // the shell does the PATH lookup and `exec "$@"` passes our argv through **unquoted and
  // unsplit**, so a device name with a space in it cannot turn into two arguments.
  function argvFor(args) {
    return ["sh", "-c", "exec \"$@\"", "sh", "punktfunk-host", "ctl"].concat(args)
  }

  // run(["approve", "3"], function (data, err) { … })
  function run(args, callback) {
    var proc = callComponent.createObject(root, {
      argv: argvFor(args.concat(["--json"])),
      cb: callback || function () {}
    })
    proc.start()
  }

  function detached(argv) {
    Quickshell.execDetached(argv)
  }

  // The console opens the way the menu entry does: an --app window (no browser chrome) at a
  // one-shot handoff URL, so it lands already logged in. Shell indirection because the ticket
  // must be minted at CLICK time — one minted at widget load would be long expired. `|| echo`:
  // a stopped host fails `ctl console-url`, and an EMPTY --app= opens a plain browser window.
  function openConsole() {
    detached(["sh", "-c",
              "exec omarchy-launch-webapp \"$(punktfunk-host ctl console-url || echo https://localhost:47992)\""])
  }

  function notify(title, body) {
    if (root.toasts) Quickshell.execDetached(["omarchy-notification-send", title, body])
  }

  Component {
    id: callComponent

    Process {
      id: proc
      property var argv: []
      property var cb: function () {}
      property string buffer: ""

      function start() { command = argv; running = true }

      // `proc.buffer`, not `parent.buffer`: inside a `StdioCollector` the `parent` scope is not
      // the Process, and QML resolves the assignment against something that has no such property —
      // "Cannot assign to non-existent property" at load, and every call silently returning
      // nothing. The first-party plugins all assign through an explicit id for this reason.
      stdout: StdioCollector { onStreamFinished: proc.buffer = text }

      onExited: function (code) {
        var payload = null
        try { payload = JSON.parse(proc.buffer) } catch (e) { payload = null }
        if (payload && payload.error) {
          root.lastError = payload.error.message
          if (payload.error.code === root.exitPin) root.pinMismatch = true
          proc.cb(null, payload.error)
        } else if (payload && code === 0) {
          root.pinMismatch = false
          root.lastError = ""
          proc.cb(payload.data, null)
        } else {
          // ctl always prints the envelope, so a parse failure means the binary is missing or the
          // host was never installed. Say that rather than "unknown error".
          var msg = payload ? JSON.stringify(payload)
                            : "punktfunk-host ctl did not answer (is the host installed?)"
          root.lastError = msg
          proc.cb(null, { code: code, message: msg })
        }
        proc.destroy()
      }
    }
  }

  // ── snapshots ────────────────────────────────────────────────────────────────────────────────
  function refresh() {
    run(["status"], function (data, err) {
      if (err) { root.state = "stopped"; root.sessions = 0; root.stream = null; return }
      root.sessions = data.active_sessions || 0
      root.pinPending = !!data.pin_pending
      root.games = data.games || []
      root.state = root.sessions > 0 ? "streaming" : "idle"
      // The Now tab shows the negotiated mode and codec, so `stream` is read here too and not only
      // by the Stats poll — otherwise the tab is blank until someone visits Stats.
      root.stream = data.stream || null
      root.audioStreaming = !!data.audio_streaming
    })
    refreshSummary()
    run(["pending"], function (data, err) {
      root.pendingDevices = (!err && data) ? data : []
      root.pending = root.pendingDevices.length
    })
    run(["pair", "status"], function (data, err) {
      if (err || !data) return
      root.armed = !!data.armed
      root.pairingPin = data.pin || ""
    })
  }

  function refreshClients() {
    run(["clients"], function (data, err) {
      if (err || !data) return
      root.nativeClients = data.native || []
      root.gamestreamClients = data.gamestream || []
    })
  }

  function refreshDisplays() {
    run(["display"], function (data, err) {
      if (err || !data) return
      root.displayPreset = (data.settings && data.settings.preset) || ""
      root.displayEffective = data.effective || {}
      root.displayPresets = data.presets || []
      root.customPresets = data.custom_presets || []
    })
  }

  // Re-reads afterwards rather than assuming: the host validates and clamps the policy it stores,
  // so what came back is the only trustworthy answer to "what is in force now".
  function setDisplayPreset(id) {
    run(["display", "preset", id], function () { root.refreshDisplays() })
  }

  // Polled, not evented: the host publishes no periodic stats event, and a bitrate that only moved
  // on a lifecycle event would be a still photograph labelled "live". `ctl status --json` measured
  // 116 ms on the Omarchy testbox — under the 150 ms threshold `ctl.rs` sets for itself — and the
  // Panel only runs this timer while the Stats tab is the one being looked at.
  function refreshStats() {
    run(["stats"], function (data, err) {
      if (err || !data) { root.stream = null; return }
      root.stream = data.stream || null
      root.sessionMode = data.session || null
      root.captureArmed = !!(data.capture && data.capture.armed)
      root.captureSamples = (data.capture && data.capture.sample_count) || 0
      root.statsSample = data.sample || null
      root.statsMeta = data.meta || null
    })
  }

  function refreshSummary() {
    run(["summary"], function (data, err) {
      root.summary = (!err && data) ? data : {}
    })
  }

  // The capture is ONE host-wide slot the web console also drives, and stopping it writes a
  // recording to disk — so it is never armed as a side effect of opening a tab.
  function setCapture(on) {
    run(["stats", "record", on ? "start" : "stop"], function () { root.refreshStats() })
  }

  // ── the event stream ─────────────────────────────────────────────────────────────────────────
  Process {
    id: watcher
    // Same PATH-lookup wrapper as `run()` — see `argvFor`.
    command: root.argvFor(["watch", "--kinds", "pairing.*,stream.*,session.*,host.*"])
    running: true
    stdout: SplitParser { splitMarker: "\n"; onRead: function (line) { root.handle(line) } }

    // `ctl watch` reconnects internally; it only exits on something a retry cannot fix (a bad pin,
    // a missing token, a host that has never run). So the timer below is a slow "has it been fixed
    // yet" poll, not a reconnect loop.
    onExited: function (code) {
      if (code === root.exitPin) root.pinMismatch = true
      root.state = "stopped"
      root.sessions = 0
    }

    // ⚠ Arm the retry from `running`, NOT from `onExited`. A process that fails to **start** — the
    // host not installed yet, which is the state a fresh box is in — never emits `exited`, so an
    // exit-only retry leaves the widget permanently dead: observed on glass, where the watcher
    // stopped for good after five failed spawns and only a shell restart brought it back.
    // `runningChanged` covers both a clean exit and a failed start.
    onRunningChanged: if (!running) retry.restart()
  }

  Timer {
    id: retry
    interval: 15000
    // Re-snapshot as well as re-watch: while the watcher was down the REST state moved on, and
    // the panel would otherwise show whatever it last saw.
    onTriggered: { watcher.running = true; root.refresh() }
  }

  function handle(line) {
    if (!line || line.length === 0) return
    var ev
    try { ev = JSON.parse(line) } catch (e) { return }

    // The display policy is not evented — it only changes when a person edits it, here or in the
    // console — so a resync re-reads it and the Displays tab re-reads it on arrival. Nothing polls.
    if (ev.kind === "ctl.resync") { refresh(); refreshClients(); refreshDisplays(); return }
    if (ev.kind === "ctl.disconnected") { root.state = "stopped"; return }

    if (ev.kind === "pairing.pending") {
      refresh()
      // The claimed name AND the fingerprint tail, always both: the name is what the device says
      // it is, the tail is the part it cannot forge. The toast never approves anything.
      var dev = ev.data || {}
      var fp = String(dev.fingerprint || "")
      var tail = fp.length > 10 ? "…" + fp.slice(-10) : fp
      notify("Punktfunk pairing request",
             "\"" + (dev.name || "a device") + "\"" + (tail ? " · " + tail : ""))
      return
    }
    if (ev.kind === "pairing.completed" || ev.kind === "pairing.denied" || ev.kind === "host.started") {
      refresh(); refreshClients(); return
    }
    if (ev.kind === "host.stopping") { root.state = "stopped"; root.sessions = 0; return }
    refresh()
  }

  Component.onCompleted: { refresh(); refreshClients() }
}
