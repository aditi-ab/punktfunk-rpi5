import QtQuick
import Quickshell
import Quickshell.Io
import "Model.js" as Model

// The plugin's data half: one `ctl watch`, the snapshots it triggers, and the
// only spawn site. The QML holds no credential and speaks no HTTPS — every
// host call is `punktfunk-host ctl`, which pins the certificate before sending
// the operator token. Reading `run()` answers whether this plugin can leak.
Item {
  id: root

  // ── published state ──────────────────────────────────────────────────────────────────────────
  property string state: "stopped"        // stopped | idle | streaming
  property int sessions: 0
  property int pending: 0
  property bool pinPending: false
  property bool armed: false
  property string pairingPin: ""
  property var pinDevices: []
  property var pendingDevices: []
  property var nativeClients: []
  property var gamestreamClients: []
  property var games: []

  // Stored preset, its expansion, and the authoritative Dedicated vs This
  // screen result. Live and lingered heads stay off this daily-use surface.
  property string displayPreset: ""
  property var displayEffective: ({})
  property var displayPresets: []
  property var customPresets: []
  property string captureMode: "dedicated"

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

  // Rolling window of the Stats poll. Filled only while the panel is open.
  // ≈ 3 minutes at the 2 s poll.
  property var history: []
  readonly property int historyMax: 90

  // Optimistic host switch: systemctl is async, so bind the knob to this
  // until the next snapshot lands.
  property bool haveDesired: false
  property bool desiredRunning: false
  readonly property bool hostEnabled: haveDesired ? desiredRunning : (state !== "stopped")

  function pushHistory() {
    var p = {
      target: root.stream ? Number(root.stream.bitrate_kbps || 0) / 1000 : null,
      sent: null, fps: null, encode: null
    }
    if (root.statsSample) {
      p.sent = Number(root.statsSample.mbps || 0)
      p.fps = Number(root.statsSample.fps || 0)
      for (var i = 0; i < (root.statsSample.stages || []).length; i++) {
        var st = root.statsSample.stages[i]
        if (st.name === "encode") p.encode = Number(st.p99_us || 0) / 1000
      }
    }
    root.history = root.history.concat([p]).slice(-root.historyMax)
  }

  function clearHistory() { root.history = [] }

  // A certificate mismatch is NOT "the host is down": something that is not our host answered on
  // the management port, and ctl refused to send the token. Surfaced separately so the panel can
  // say so instead of showing a plausible-looking "not running".
  property bool pinMismatch: false
  property string lastError: ""

  readonly property int exitPin: 4

  // Quickshell's Process does not search PATH. The ctl and local-helper paths
  // share this wrapper so lookup never rejoins or re-quotes an argv.
  function spawnArgv(bin, args) {
    return ["sh", "-c", "exec \"$@\"", "sh", bin].concat(args)
  }

  function argvFor(args) {
    return spawnArgv("punktfunk-host", ["ctl"].concat(args))
  }

  // run(["approve", "3"], function (data, err) { … })
  function run(args, callback) {
    var proc = callComponent.createObject(root, {
      argv: argvFor(args.concat(["--json"])),
      jsonEnvelope: true,
      cb: callback || function () {}
    })
    proc.start()
  }

  function runArgv(bin, args, callback) {
    var proc = callComponent.createObject(root, {
      argv: spawnArgv(bin, args),
      jsonEnvelope: false,
      cb: callback || function () {}
    })
    proc.start()
  }

  function detached(argv) {
    Quickshell.execDetached(argv)
  }

  function detachedBin(bin, args) {
    detached(spawnArgv(bin, args))
  }

  function setHostEnabled(on) {
    root.haveDesired = true
    root.desiredRunning = !!on
    detachedBin("systemctl", ["--user", on ? "start" : "stop", "punktfunk-host.service"])
  }

  function setCaptureMode(mode) {
    var selected = Model.captureModeStatus(mode)
    if (!selected) return
    runArgv("punktfunk-omarchy", ["mode", selected], function (output, err) {
      if (!err) root.refreshCaptureMode()
    })
  }

  // The console opens at a one-shot login page under $XDG_RUNTIME_DIR, so it lands already logged
  // in with no bearer URL in argv. Shell indirection because the ticket must be minted at CLICK
  // time — one minted at widget load would be long expired. `|| echo`: a stopped host fails `ctl
  // console-url`, and an EMPTY --app= opens a plain browser window.
  function openConsole() {
    detached(["sh", "-c",
              "exec omarchy-launch-webapp \"$(punktfunk-host ctl console-url || echo https://127.0.0.1:47992)\""])
  }

  Component {
    id: callComponent

    Process {
      id: proc
      property var argv: []
      property bool jsonEnvelope: true
      property var cb: function () {}
      property string buffer: ""
      property string errorBuffer: ""

      function start() { command = argv; running = true }

      // A collector's parent scope is not the Process. Explicit ids keep both
      // streams attached to this invocation instead of silently losing output.
      stdout: StdioCollector { onStreamFinished: proc.buffer = text }
      stderr: StdioCollector { onStreamFinished: proc.errorBuffer = text }

      onExited: function (code) {
        if (!proc.jsonEnvelope) {
          if (code === 0) {
            root.lastError = ""
            proc.cb(proc.buffer, null)
          } else {
            var commandError = proc.errorBuffer.trim()
            if (!commandError) commandError = "command exited with code " + code
            root.lastError = commandError
            proc.cb(null, { code: code, message: commandError })
          }
          proc.destroy()
          return
        }
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
  function applyPairingWindow(data) {
    var next = Model.pairingWindow(data)
    root.armed = next.armed
    root.pairingPin = next.pairingPin
    root.pinDevices = next.pinDevices
    root.pinPending = next.pinPending
  }

  function clearPairingState() {
    applyPairingWindow(null)
    root.pendingDevices = []
    root.pending = 0
  }

  function refresh() {
    run(["status"], function (data, err) {
      if (err) {
        root.state = "stopped"
        root.sessions = 0
        root.stream = null
        if (!(root.haveDesired && root.desiredRunning)) root.haveDesired = false
        return
      }
      root.haveDesired = false
      root.sessions = data.active_sessions || 0
      root.games = data.games || []
      root.state = root.sessions > 0 ? "streaming" : "idle"
      root.stream = data.stream || null
      root.audioStreaming = !!data.audio_streaming
    })
    refreshSummary()
    run(["pending"], function (data, err) {
      root.pendingDevices = (!err && data) ? data : []
      root.pending = root.pendingDevices.length
    })
    run(["pair", "status"], function (data, err) {
      if (err || !data) {
        root.applyPairingWindow(null)
        return
      }
      root.applyPairingWindow(data)
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
    refreshCaptureMode()
    run(["display"], function (data, err) {
      if (err || !data) return
      root.displayPreset = (data.settings && data.settings.preset) || ""
      root.displayEffective = data.effective || {}
      root.displayPresets = data.presets || []
      root.customPresets = data.custom_presets || []
    })
  }

  // The helper includes host.env overrides, so neither the display snapshot
  // nor the requested action is authoritative.
  function refreshCaptureMode() {
    runArgv("punktfunk-omarchy", ["mode", "--status"], function (output, err) {
      if (err) return
      var mode = Model.captureModeStatus(output)
      if (!mode) {
        root.lastError = "punktfunk-omarchy mode --status returned an invalid result"
        return
      }
      root.captureMode = mode
    })
  }

  // Re-reads afterwards rather than assuming: the host validates and clamps the policy it stores,
  // so what came back is the only trustworthy answer to "what is in force now".
  function setDisplayPreset(id) {
    run(["display", "preset", id], function () { root.refreshDisplays() })
  }

  // Polled, not evented: the host publishes no periodic stats event. The
  // panel runs this only while it is open.
  function refreshStats() {
    run(["stats"], function (data, err) {
      if (err || !data) return
      root.stream = data.stream || null
      root.sessionMode = data.session || null
      root.captureArmed = !!(data.capture && data.capture.armed)
      root.captureSamples = (data.capture && data.capture.sample_count) || 0
      root.statsSample = data.sample || null
      root.statsMeta = data.meta || null
      // After the assignments, never before: the point appended is the one just read.
      root.pushHistory()
    })
  }

  function refreshSummary() {
    run(["summary"], function (data, err) {
      root.summary = (!err && data) ? data : {}
    })
  }

  // Stopping a capture writes a recording to disk, so it is never armed as
  // a side effect of opening the panel.
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
      if (!(root.haveDesired && root.desiredRunning)) root.haveDesired = false
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

    // Display policy is not evented, so a resync re-reads it. Nothing polls it.
    if (ev.kind === "ctl.resync") { refresh(); refreshClients(); refreshDisplays(); return }
    if (ev.kind === "ctl.disconnected") {
      root.state = "stopped"
      root.clearPairingState()
      return
    }

    // A knock only refreshes the badge. The toast — with Approve/Deny — is the `pairing-pending`
    // hook's job; a second one here made every request ring twice.
    if (ev.kind === "pairing.completed" || ev.kind === "pairing.denied" || ev.kind === "host.started") {
      refresh(); refreshClients(); return
    }
    if (ev.kind === "host.stopping") {
      root.state = "stopped"
      root.sessions = 0
      root.clearPairingState()
      return
    }
    // A graph that spans the gap between two sessions draws a line between numbers that were never
    // related. The window belongs to one stream.
    if (ev.kind === "stream.stopped" || ev.kind === "session.ended") clearHistory()
    refresh()
  }

  Component.onCompleted: { refresh(); refreshClients() }
}
