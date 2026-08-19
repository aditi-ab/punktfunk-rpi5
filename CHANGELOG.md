# Changelog

Protocol, ABI, driver and embedder detail, one section per stable release, newest first.

This is the **technical** half of a release. The other half — what changed for people who *use*
Punktfunk — is `docs/releases/vX.Y.Z.md`, and it deliberately contains no internal names. The two
were one document through v0.24.0; they split at v0.25.0 because the engineering section had grown
long enough to bury the user-facing half it was appended to. See `docs/releases/README.md`.

If you embed `punktfunk-core`, package Punktfunk, or write a plugin, this file is for you. Start
with the version table of the release you are moving to, then read **Breaking changes**.

---

## v0.31.0

90 commits since v0.30.0 (65 non-merge).

Nothing versioned moves. `WIRE_VERSION` stays **2**, the C ABI stays **24** — `include/punktfunk_core.h`
is byte-identical to the v0.30.0 tag — the driver protocol, gamepad channel and plugin index schema
are all unchanged, and no `trust::Settings` field, capability bit or control-message type byte was
added. Every 0.30.x host, client, driver and plugin keeps interoperating in both directions, with no
re-pairing.

What did move is beneath the versioned surfaces, and three parts of it are worth a packager's or
embedder's attention: the Linux host package installs **three new system files** (a udev rule, a
WirePlumber policy and an ALSA UCM drop-in) that the DualSense audio path depends on; the Linux
desktop-audio capture **flipped topology by default** (`PUNKTFUNK_STREAM_SINK` unset now means a
host-owned `null-audio-sink`, with `=stream` a one-release escape hatch to the 0.30 shape); and the
Android app's Compose console is **deleted** — `pf-console-ui` over Skia/GL is now the console on all
three ABIs, which removes the Compose screenshot scenes.

### Versions

| | v0.30.0 | v0.31.0 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged |
| C ABI | 24 | **24** | unchanged — `include/punktfunk_core.h` is byte-identical to the v0.30.0 tag; the only new `pub` items in `punktfunk-core` are three RT-safe DSP helpers (`crossfade_insert`, `pcm::raised_cosine_tail`, `pcm::raised_cosine_head`), Rust-only, no `pub const` for cbindgen to pick up |
| Rust edition | 2024 | **2024** | unchanged |
| MSRV (`rust-version`) | 1.85 | **1.85** | unchanged |
| Workspace crate dirs | 27 | **27** | unchanged (39 `[workspace] members`, also unchanged) |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3); `pf-driver-proto` shows no diff against the v0.30.0 tag |
| Windows virtual-gamepad channel | 3 | **3** | unchanged |
| Plugin index schema | 1 | **1** | unchanged |
| Host event schema | 1 | **1** | unchanged (`punktfunk-host/src/events.rs`) |
| `api/openapi.json` | 0.29.0 | **0.29.0** | unchanged — no management-API surface moved this cycle; both copies (`api/` and `docs-site/public/`) are byte-identical to each other and to the tag |
| gamescope patch level (`+pfhdrN`) | 8 | **8** | unchanged; no new patch files. ⚠ `packaging/gamescope/PKGBUILD` still says `pfhdr7` — pre-existing at v0.30.0, not a regression this cycle, but the Arch package builds a binary the host's `>= 8` probe rejects for the keymap path |
| `@punktfunk/host` (SDK) | 0.1.4 | **0.1.5** | cut — `sdk/src/config.ts` and `runner-cli.ts` carry the `mgmt-endpoint` fix below, and plugins resolve the SDK from the registry, so it could not reach them until it shipped |
| `@punktfunk/plugin-kit` | 0.4.2 | **0.4.3** | cut, for the two `sync-engine.ts` changes that cannot reach a plugin any other way: `minInterval` (below) and the always-apply sync reasons (`startup`/`manual` publish even when the fingerprint matches, so a host-side art drop is recoverable by restarting rather than by deleting the plugin's cache). Note the registry skips 0.4.2: `plugin-kit-v0.4.2` was tagged but its publish never landed, and the tag is left where it is rather than moved |

⚠ The SDK and plugin-kit version independently of the app (`sdk-v*` / `plugin-kit-v*` tags,
`sdk-publish.yml` / `plugin-kit-publish.yml`); this release commit does not bump them. Both have
unpublished code changes, called out in the table so they are cut deliberately rather than
discovered.

### ⚠ Breaking changes

**None on any versioned surface.** No wire change, no C ABI change, no driver-protocol change, no
plugin-contract change. Four things are worth attention anyway; none breaks a build:

- **`refactor(android)!` — the Compose console is deleted.** `pf-console-ui` (the Skia shell the
  desktop session binary draws) is now Android's console on arm64-v8a, x86_64 **and** armeabi-v7a;
  the gate is simply "does the native host exist", and where it does not a controller drives the
  touch UI through focus. ~6.5 kLOC of `GamepadHome`, `GamepadSettingsScreen`,
  `GamepadAddHostScreen`, `GamepadDialogs`, `HomeTiles`, the console halves of `LibraryScreen`,
  `ConnectOverlay`/`ConnectTakeover`, the `gamepadUi` branches of `ConnectScreen`/`ConnectPrompts`/
  `AdaptiveDialogs`, `App.kt`'s `GamepadShell`/`GamepadScreen` and their tests are gone. The `!` is
  for the **store-screenshot surface**: the Compose console's marketing scenes cannot be rendered by
  Roborazzi any more (the shell draws over native GL); its shots come from the desktop screenshot dump
  or a device capture. Sysprop `debug.punktfunk.console_backend=compose` is meaningless; `=none`
  still forces the touch UI on glass.
- **Linux desktop-audio capture topology flipped by default** — see the audio section. `=stream`
  restores 0.30 for **one release only**.
- **Hyprland / sway: `topology: exclusive` now does what it says.** Both backends accepted it,
  echoed it as the session's effective topology, and dropped it with a warning; because `auto`
  resolves to Exclusive on any unpinned host, the *default* policy on every auto-detected Hyprland
  or sway box was an Exclusive that behaved as Extend. Operators who relied on that get their
  monitors disabled for the session now (closes #284).
- **Three new system files in the Linux host package** — the DualSense audio path does not work
  without them. Downstream repackagers: see the packaging section.

### DualSense audio and haptics on Linux: five faults, and the files they needed

The whole in-game path — GE-Proton's haptic router → the pad's ALSA card → the voice coils — had
never once worked against our virtual pad. In wire order:

- **`usbip`: the calibration feature report was 42 bytes; `hid-playstation` asks for 41.** On a USB
  backend an over-long reply is not truncated: the kernel treats it as hostile and tears down the
  connection, not the transfer — the pad vanished ~400 ms after enumerating, and the dmesg order made
  the teardown look like the cause. Three changes so the trap is not left set: the constant is 41 and
  all three feature-report sizes are pinned by test; `clamp_reply` clamps every reply to the requested
  length in the transport and drops any payload a handler returns on an OUT (the kernel never reads
  one; those bytes would misframe every following PDU); `DualSenseUsbip::open` waits for the kernel
  to actually bind a HID driver before reporting success (vhci attach succeeds immediately and
  enumerates asynchronously), so bring-up faults return `Err` and the uhid fallback catches them.
  New `PUNKTFUNK_USBIP_TRACE` (both socket directions to disk) and `scripts/usbip-trace-analyse.py`.
- **`usbip`: every non-ISO OUT was answered with an empty buffer, i.e. `actual_length = 0`.** vhci
  copies that field verbatim into the URB's actual length; the driver returned 0 as the write's byte
  count; Wine's bus driver reads 0 as failure and prints the thread's *stale* errno — so the ENOENT /
  EINVAL / EAGAIN in the GE logs were never kernel verdicts. New
  `UsbIpResponse::usbip_ret_submit_out_success(header, accepted)`; the debug assertion now pins
  "OUT carries no buffer", not "OUT claims 0"; two wire-byte tests pin both directions. **The Steam
  Controller 2 shares this handler.** `usbip-trace-analyse.py` had flagged *any* nonzero OUT
  actual_length as a desync — the rule that would have hidden this bug — and now flags an OUT reply
  claiming more than it was sent, or 0 against a non-empty write.
- **`usbip`: ISO completions were paced by relative sleeps**, so timer slop, socket I/O and lock waits
  accumulated per transfer: the pad's clock ran ~26 % slow (~35,700 frames/s against 48 kHz), its PCM
  backed up into dropouts, and because completion *is* the pad's audio clock, on the test box the pad
  sink became the graph driver and pulled desktop capture to 50 % delivery. Now a per-endpoint
  absolute deadline ledger (a stall > 20 ms re-anchors instead of fast-forwarding a burst); measured
  after: 48,005 frames/s. Two paused-clock tests pin the rate and the re-anchor.
- **`usbip`: the capture forwarded the pad's hardware quad as the wire's speaker pair.** Hardware
  is HP-L, HP-R+mono-speaker, coil-L, coil-R; the wire puts the speaker pair first. Now: the speaker
  channel duplicated across the wire's speaker pair, coils passed through, HP-L dropped. The
  stream-sink (uhid) capture path already emitted the logical layout and is unchanged.
- **`usbip`: `iSerialNumber` was the literal `"Serial"`.** A real DualSense reports none, ALSA bakes it
  into the card id (`…Wireless_Controller_Serial-00` vs `…Wireless_Controller-00`) and PipeWire
  carried it into every node name and `device.serial`. Cleared. Explicitly *not* a fix for anything
  observed broken — GE's winepulse leg matched the placeholder — and *not* a UCM-selection fix
  (alsa-ucm-conf keys on `${CardComponents}`, `USB054c:0ce6`).
- **The pad's ALSA card was root-only.** It is created mid-session-bringup with no seat session
  active, so logind's ACL never materialises; WirePlumber's probe got EACCES and the card never
  appeared in PipeWire at all. `scripts/60-punktfunk.rules` gains two `SUBSYSTEM=="sound"` rules for
  `054c:0ce6` / `054c:0df2` (`GROUP="input" MODE="0660" TAG+="uaccess"`), matching physical pads too.
  Verified live on Bazzite f44.
- **The DualSense's only playback route was a 1-channel `Default__Speaker__sink`**, from which
  GE-Proton mints its synthetic endpoint, and *Marvel's Spider-Man Remastered* overruns it ~74 s in
  (`EXCEPTION_ACCESS_VIOLATION`, write; the copy loop past the frame count, 5206/5207 vs 5034 — a
  game/GE bug on a code path that only exists when the mono sink does). Fix: delete the sink. New
  ALSA UCM drop-in `scripts/alsa-ucm2/USB-Audio/conf.d/{054c-0ce6,054c-0df2}.conf` +
  `scripts/alsa-ucm2/USB-Audio/Punktfunk/DualSense-PS5-Haptic{,-HiFi}.conf` raises a `SpeakerHaptic`
  device at playback priority 200 against `Speaker`'s 100, so the card takes the 4-channel HiFi
  profile and the mono sink never exists. Shipped **without** replacing a file `alsa-ucm-conf` owns:
  `USB-Audio.conf` ends with an unconditional optional include of `conf.d/{vid}-{pid}.conf`
  (verified against alsa-lib source; hook and DualSense profile both since 1.2.15). New CI guard
  `scripts/ci/check-dualsense-ucm.sh` runs the chain on a real distro tree via UCM's card-less
  `conf.virt.d`, negative-tested both ways. **NixOS is not covered** (no `/usr/share/alsa/ucm2` to
  drop into).
- **WirePlumber met every new pad card at `default-sink-volume` 0.4 — cubed, i.e. −23.88 dB — and
  both ends minted one**, so haptics reached the coils at 0.064² = −47.8 dB (field-measured −48).
  Client: `pin_sink_volume` from `correlate_pad_sink` at every pick (skipped for the `split_parent`
  pick). Host: new `audio/linux/pad_card_volume.rs`, started when `PadUsbCapturer::open` succeeds
  (the host half matters because `pad_usb` captures at the ISO OUT endpoint, downstream of this
  sink), retrying 15 s because the USB device is live before its ALSA card is; only sinks of a
  DualSense **card** are touched (`device.id` keeps it off the host's own minted pad sink). Neither
  end restores on exit, deliberately. New `PUNKTFUNK_PAD_SINK_VOLUME=0` disables both ends for
  bisecting. Both pins unit-tested for one unity float per channel — PipeWire silently ignores a
  `channelVolumes` whose length mismatches the port count.
- **`scripts/60-punktfunk-dualsense.conf`** — a new WirePlumber policy installed to
  `/usr/share/wireplumber/wireplumber.conf.d/` by rpm/deb/arch/nix: `node.always-process` + no
  suspend on the pad's `alsa_output` (GE opens the backing device raw when it is free, then hits
  "busy" against its own handle and spins a 100 Hz refresh loop — SteamOS never shows this because
  PipeWire always holds the device there), and `priority.driver = 0`. **Zero, not one**: the field is
  unsigned and a driver is skipped only when `<= 0`; at 1 the pad was merely *last*, and last is still
  elected whenever nothing above it qualifies — the ordinary in-session state on a host that has
  claimed its own sink as default and idled the real card. A second rule sets `priority.driver = 0`
  on the same cards' `alsa_input` (in the Pro Audio profile that node carries 2600 and clocked a
  reporter's whole desktop session with nothing linked to it). The rule's first landing duplicated
  its `%files` line into `%install`, which killed every RPM build on main for a few hours (fixed same
  day, no release affected).
- **`0xD1` lane split:** speaker = Opus `Application::Audio` @ 96 kbps (~120 B / 10 ms frame),
  haptics = `Application::LowDelay` @ 64 kbps CBR, unchanged.
- **`punktfunk-session --pad-audio-test`** now prints the effective `pad_speaker` / `pad_haptics`
  before the tone (the capability is never advertised when the toggle is off, so no later log line can
  catch it); the Android settings row states its default. Android is the one client defaulting pad
  speaker **off**; `pf_client_core`'s `default_pad_speaker` is `"pad"` and always was.

### The Linux desktop-audio capture drives its own graph group

The stream sink was a `pw_stream` wearing `media.class = Audio/Sink`. A stream is structurally a
follower, so its group had no clock and PipeWire assigned it to the highest-priority *running*
driver on the box. On a reporter's host that was a DualSense forwarded over VirtualHere in the Pro
Audio profile — never suspended, nothing linked, its frame counter a kernel stub logging "not yet
implemented" and returning 0 ~1900×/s. Not xruns: 11 errors in 15 min, wait never past 111 µs; the
loss was *between* cycles — 3.9 delivery holes/s, worst 142 ms, **15.4 % synthesized silence** over
a 15-minute session.

Now a `support.null-audio-sink` adapter created on our own connection, captured through its monitor
(the same object `pactl load-module module-null-sink` creates). Three load-bearing properties:
`node.passive` on the monitor tap (idle between sessions, so the null sink's timer parks — the
objection that kept `node.always-process` off the old stream sink); `node.force-quantum`, not
`node.latency` (a driver's quantum is the smallest follower latency rounded **down** to a power of
two under the default `default.clock.power-of-two-quantum`, which is why the 240-frame ask has been
served as **128** — 2.67 ms callbacks, not the 5 ms it is designed around — on every stock Linux host
since the capture was written; force-quantum skips the rounding and forces nothing on anyone else,
since this sink drives only its own group); and `node.dont-fallback` **with** `node.linger`, never
one alone (WirePlumber 0.5 reads dont-fallback alone as licence to destroy the stream when its target
is not visible). Routing claim, capture callback, stats line and everything downstream untouched.
`PUNKTFUNK_STREAM_SINK`: unset = new topology, `stream` = 0.30's (one release), `0` = the legacy
default-sink-monitor follower. Documented at last in `configuration.md`, with a new troubleshooting
section on the `punktfunk-audio-…` recording stream and on another device clocking your capture.

Around it, from the same 2026-08-14/17 field logs:

- The host binds its own node and reads `node.driver-id` from its `info` event (a node-id→name map
  from the registry): on change, `audio capture graph driver` names the clocking node — WARN in the
  null-sink mode (exactly one right answer), INFO in the legacy topologies (they borrow a clock by
  design).
- `CaptureStats::observe_gap` is now the one accounting behind both feeds (Linux callback cadence and
  the Windows discontinuity flag) and buckets holes at <20 / <50 / <100 / ≥100 ms — the client
  concealment edges. Both capture lines print `gap_hist=a/b/c/d missing_ms=`; the sum closes the
  arithmetic against `delivered_pct`. The Windows loopback **reader** thread now takes
  `boost_thread_priority(true)` like the paced sender it feeds.
- **The pacer's schedule was wall clock; the source was not.** A missed 2.7 ms cycle is below the gap
  counter's floor and the infill threshold, so the schedule kept the debt and repaid the next ≥ 10 ms
  hole as a burst of (lag + 10)/5 silence frames (field: 33–72 % departures late, worst 99 ms,
  re-anchors 0). The infill decision now sees schedule lag; `after()` follows the real quantum
  (`InfillPolicy::note_quantum`) — one chunk plus one frame, never under two frames; a slot whose
  backlog exceeds one chunk plus one frame sends a second frame in the same slot (at most two), since
  a fast source clock could otherwise only grow the backlog — 5 ms of host latency per 50 s at
  100 ppm. Holes fade out over 1 ms (`pcm::raised_cosine_tail`) and the first real frame after fades
  in (`raised_cosine_head`).

### The client jitter ring can now grow without de-priming

`JitterStep::insert_front` mirrors `drop_front`: when the sync loop wants more than the adaptive
target and the depth EWMA has sat > `INSERT_MARGIN_MS` below the request for `INSERT_SUSTAIN_MS` of
consumed audio, duplicate one frame at the front, crossfaded (`crossfade_insert`, the RT-safe twin
of `crossfade_drop`). Sync-only, primed-only, below-target-only. `hollow` is judged against the
**adaptive** target, never the sync request — the bug was that a ≥ 10 ms sync request read as hollow
on the next callback and the next late packet cost 15–60 ms of silence, since ~0.24/0.25. Margin is
half the sync loop's ±10 ms deadband (a margin at or above it would leave every request it is allowed
to make unanswered). Also fixes `crossfade_drop`'s seam: the fade-out source is now the continuation
of the sample the device just played, not the tail of the discarded region — a hard-cap trim stepped
2,688 samples where it now stays under 17. Wired into the PipeWire, WASAPI and AAudio rings
(`PlaybackVitals.inserts`, `drift_inserts=` on the 10 s lines) and ported line for line to the Swift
ring (`insertOneFrame()`, `AudioRingDriftTests` carrying the same vectors). No new `pub const`; the
C header is unchanged.

Beside it: the Linux desktop client's playback stream now connects with `RT_PROCESS` (it ran on the
main-loop thread at nice 0, and when late PipeWire rendered silence for our node and moved on — an
underrun no counter saw); the ring is pre-reserved so `extend` never reallocates on the RT loop; new
`audio_vitals::PlaybackVitals` printed from the decode thread on wall clock. New `audio_rt` module
raises the decode, pad-audio, PipeWire-loop and Linux mic threads: `setpriority` where `RLIMIT_NICE`
allows → inside a Flatpak the `org.freedesktop.portal.Realtime` portal → else rtkit
`MakeThreadHighPriorityWithPID`. The split is `module-rt`'s and not optional: rtkit-daemon has no
PID-namespace translation (verified on the Deck, rtkit 0.14), so a direct call from a sandbox is
ENOENT; the portal maps pid/tid. Never setcap / `SCHED_RR`. Windows: MMCSS "Pro Audio" +
`THREAD_PRIORITY_HIGHEST` on the render and mic loops. Acceptance on the Deck: `ps -eLo
cls,rtprio,ni,comm` shows the decode thread at nice −10 after connect.

The client log ring drops DEBUG/TRACE from `cros_codecs` (its WARN+ still lands) and normalizes
`log`-bridge events to their real target: a dozen DPB lines per frame at 120 fps last three seconds
in a 4,096-line ring — a 2026-08-17 Deck bundle read "2,037,456 older lines evicted". `Cargo.lock`
gains two direct deps already in the graph.

### Android: `pf-console-ui` is the console, presented through `ASurfaceControl`

- **`pf-client-core` un-gated for Android** (trust::Settings, known-hosts store, profiles model,
  deep links, the library *model*; the ureq fetches stay desktop), with `audio_format`,
  `decoder_pref`, `menu_nav` (`MenuEvent`/`MenuNav`/`PadInfo`) and `console` (`OverlayAction`,
  `PointerInput`, `SessionPhase`) split out and re-exported. `pf-console-ui`: Vulkan overlay + SDL
  event path behind the default `vulkan-overlay` feature (clients/session unchanged); a `Key` enum
  replaces SDL scancodes; a `SettingsStore` seam (desktop = the file, `SnapshotStore` across a
  language boundary); `Viewport{width,height,insets,scale}`; `Platform` filters the settings rows;
  `ConsoleOptions`; a portable `Console` driver. skia-safe features are target-specific: desktop
  `jpegd-jpege-pdf-textlayout-vulkan` (the flatpak pin), Android `gl-jpegd-jpege-pdf-textlayout`.
  Model types derive serde — the wire IS the model. `MenuNav` gains the stick hysteresis
  (`MENU_RELEASE = 0.3`) both the Apple and Android shells had grown on glass.
- **`clients/android/native/src/console/`**: hand-declared EGL binding, Skia GL `DirectContext` over
  FBO 0, one render thread paced by `eglSwapBuffers`, ~28 `nativeConsole*` JNI seams; a run of GL
  setup failures ends the render thread through the normal release path, which raises the
  `SkiaConsole.healthy` handover to the touch UI. `SkiaConsoleShell` (SurfaceView + lifecycle,
  insets = systemBars ∪ displayCutout in surface px, system bars hidden transiently while the console
  is up, phone density floor **0.6 → 0.75**, pad probes into the shared `MenuNav`, remote D-pad,
  hardware keys, Back as B, touch as pointer). Pad-listener slot is a **stack** with removal by
  identity (a leaving Controllers/Licences page used to null the console's claim). Android-only
  settings rows ride `Settings::extra` `android.*` keys; `row_on()` keeps them off the desktop list.
  New `ConsoleCmd::PadAction { action, pad_key }` (`sc2_bluetooth`, `sc2_usb`, `ds_usb`, rumble,
  pad-audio self test); `PlatformScreen::Controllers` removed (the mechanism stays for Licences);
  `PadInfo` gains detail line / forwarded / rumble. Detail band 84 → 64 units; the grid's two-column
  minimum shrinks covers instead of clipping.
- **Skia prebuilts** for all three ABIs come from `unom/skia-binaries` release **0.99.0** on
  git.unom.io (R2-backed), mirroring rust-skia's `{tag}/{key}` layout; the armv7 archive
  (`a25a0fdb7d90429aa2d1-armv7-linux-androideabi-gl-jpegd-jpege-pdf-textlayout`, sha256
  `4867856b…`) is built by us since rust-skia publishes none. GitHub is out of the Android build path;
  `-PskiaBinariesUrl` / `SKIA_BINARIES_URL` remain as overrides.
- **Present path:** the codec renders into an `AImageReader`; frames are composited onto an
  `ASurfaceControl` layer via a transaction carrying a desired present time, and completion reports
  the real latch time and the previous buffer's release fence — so the panel period is learned from
  real latches (Android down-rates a game process's vsync callbacks; the old presenter could learn 60
  on a 120 Hz panel) and the frame budget is bounded by real completions. `ASurfaceControl` /
  `ASurfaceTransaction` are not in ndk-sys 0.6, so `surface_control.rs` hand-declares them and
  resolves via `dlsym` from `libandroid.so` (all API 29, above minSdk 28), same pattern as `adpf.rs` /
  `vsync.rs`. Memory safety does not rest on the fences (an `AImage` keeps its buffer alive through
  SurfaceFlinger's own reference; a mishandled fence is at worst a tear). **Default**; auto-fallback
  to the SurfaceView presenter, byte-for-byte unchanged, on API < 29 or any init failure; escape hatch
  `debug.punktfunk.present_backend=surfaceview`. The layer is sized to the view's on-screen pixels,
  not the window buffer (which is reported in a rotated/scaled space — 1260×567 for a 2800×1260
  stream, drawing into the top-left 45 %). The present-time grid uses the mode table's seed period
  for spacing and the last real latch only for phase (learning the period from latches was
  self-fulfilling and locked the panel at 60). On glass at 2800×1260@120: e2e p50 30 → ~18 ms,
  skipped 40–50/s → 0. Whether the panel *holds* 120 is the OEM's LTPO governor — measured: no
  app-side API (`preferredDisplayModeId`, `preferredRefreshRate`, the layer rate vote,
  `frameRatePowerSavingsBalanced`) raises the render-range floor — so the ineffective pins were
  removed again and `pf.present` gained the cadence loop's late-permille / jitter / cushion /
  re-anchors / qDepth.

### Hyprland / sway: `topology: exclusive` (closes #284)

`exclusive` disables the operator's outputs for the session and restores them when the display
group's last member is torn down, through the same registry hand-off KWin uses (the compositor never
sees zero enabled outputs; a sibling session's desk is never re-enabled under it). The disable filter
is group-aware — enabled, not ours (`PF-<pid>-<n>` on Hyprland, the `HEADLESS-` prefix on sway), not
managed. **The Hyprland restore is `hyprctl reload`, and that is measured, not chosen**: re-applying
the head's own mode/position/scale does not undo a disable (probed 2026-08-18 against 0.56.2
hyprlang and 0.55.4 Lua — every targeted form was accepted at exit 0 and changed nothing, including
`,enable`, `preferred,auto,1`, `monitorv2 disabled=false`, `keyword unset monitor`, the Lua
`disabled = false`, `dispatch dpms on`, `forcerendererreload`); a runtime rule is additive and the
disable keeps winning. Disable is spelled per config era (`keyword monitor <n>,disable` under
hyprlang; `hl.monitor{ output = "<n>", disabled = true }` under Lua) and confirmed by **read-back**,
not exit status. `hyprctl_dispatch` now also matches "can't" (the Lua manager's "keyword can't work
with non-legacy parsers"). `primary` stays extend and warns distinctly. ⚠ **The sway half is not
exercised on a live sway** — no box in the fleet runs one; both argv shapes are pinned by tests and
the read-back turns a wrong guess into a warning naming the outputs. Six new unit tests.

### Gaming Mode takeover: the mask was the relogin storm

On an SDDM-autologin box the runtime mask the takeover laid sat in SDDM's relogin path, so every
autologin failed in milliseconds and `Relogin=true` has no backoff: 962 logind sessions in 3.7 min,
system buttons re-scanned 5,688×, udev `change` at ~20/s, iio-sensor-proxy crash-looping ~16
starts/s, load 26 on 12 cores — and Wine's bus driver, re-enumerating udev per event, read the pad at
~1.4 Hz. `dm_plan` loses its `mask` input and `dm_survives_masked_unit`; the mask is laid **only after
the stop has landed** and every restore path unmasks before restarting; a planned DM stop that does
not land now **fails the takeover** and the caller degrades to ATTACH. `skip` is `!any_live` on every
flavor; `any_live` now counts `deactivating` and `reloading`. New `DmHelperError::shape()`;
`watch_for_relogin_storm()` (two `read_dir`s of `/run/systemd/sessions` 5 s apart, ERROR above 1/s,
detect-only); `systemctl_system` captures stderr at DEBUG (the "requires interactive authentication"
line was going to the journal on the *successful* path). `cargo test -p pf-vdisplay --lib gamescope`
52 passed, 1 ignored.

### Windows host: two session-killers

- **`untune_process` logged from a TLS destructor.** By then `tracing`'s own thread-local state can be
  gone; the log call panicked, and a panic escaping a TLS destructor aborts. The panic hook then hid
  the evidence — it logged through the same framework and panicked the same way, and a panic inside
  the hook is a case where std deliberately does not format the message (the field log: a location, a
  blank line, "thread panicked while processing panic. aborting."). The service manager restarted the
  host ~6 s later, so it read as a reconnect. `untune_process` no longer logs (still atomic under the
  refcount lock); the panic hook writes straight to the `LogRing` (`OnceLock` + `Mutex`, TLS-free;
  `thread::current()` and `Backtrace::force_capture()` verified safe during TLS destruction).
  Reproduced standalone on 1.96.0, byte-identical to the field log.
- **A Windows launch is a hand-off, and 0.30 read its exit as the game's.** `explorer.exe
  "playnite://…"`, `Steam.exe "steam://…"` and shell app-folder links spawn a forwarder that quits a
  second later (launcher already running) or *becomes* the launcher (it was not); the shim window that
  guards this was skipped for hint-less titles — the one shape that needs it — so the lease reported
  running, then the forwarder's exit closed the connection. The forwarder was also a termination
  target. `WinRecipe::owns_game` records which recipe lines start the game (only `gog`, `command` and
  a plugin's own recipe) and which forward; a forwarder's pid is dropped; the shim window applies to a
  bare child or pid whatever the spec holds; giving up on tracking lands on `GameState::Untracked`
  instead of `launching` forever. Fixture in `a_pid_only_launch_reports_its_exit` widened 4 → 8 s
  (it passed only because of the bug); new ignored test drives the field report.

### Everything else an integrator might notice

- **`mgmt-endpoint` is followed everywhere.** `PUNKTFUNK_MGMT_BIND` moved off 47990 left every plugin,
  the runner's log shipper and the tray dialing a dead port (task Running, plugins never registering,
  empty library, "no logs at all"). `sdk/src/config.ts::publishedMgmtUrl` reads
  `<config_dir>/mgmt-endpoint`; `resolveConfig` uses it after `PUNKTFUNK_MGMT_URL` and before the
  default; `runner-cli.ts` exports it into `PUNKTFUNK_MGMT_URL` before any plugin loads (older
  vendored SDK copies follow too). New `pf_paths::published_mgmt_port`; `punktfunk-tray` depends on
  `pf-paths` and its `mgmt_port` is `Option<u16>` — `None` re-reads the file every poll. SDK 83 tests
  (4 new). **Unpublished — `sdk-v0.1.5` owed.**
- **`scripts/windows/scripting-run.cmd`** redirects the runner's stdout+stderr to
  `%ProgramData%\punktfunk\plugin-state\runner.log` (previous run rotated to `.1`; writability probed
  with `copy /y nul`; no `goto`, the file is LF). Verified by reading only.
- **`@punktfunk/plugin-kit`: `SyncSettings.minInterval`** (optional; `LibraryPluginDef.minInterval`
  overrides), default `DEFAULT_FS_CHANGE_MIN_INTERVAL` = 30 s — a floor on top of the 3 s debounce,
  which cannot bound the *rate* under sustained churn (`plugin:steam sync (fs-change)` 102× in
  27 min). Changes inside the hold coalesce into one trailing sync. **Unpublished — `plugin-kit-v0.4.3`
  owed.** Narrowing the Steam plugin's watch set lives in the steam plugin repo.
- **Nix binary cache at `https://nix.unom.io`** (`nix.yml` third tier: build Rust packages +
  gamescope, sign, publish on every main push; a release needs no new trigger since `Cargo.toml` is
  in the path filter). Only punktfunk's own store paths (~300 MB per publish); the step asserts every
  output matches the name filter; NARs before narinfos, rsync without `--delete`. New
  `packaging/nix/server/{Caddyfile,compose.production.yml,prune.sh}` (a `caddy:2-alpine` static tree
  on unom-1 beside the flatpak repo) and `scripts/setup-nix-cache.sh` (five stages; the secret key is
  shown once and never written to disk; four stages after #318, which also made it detect an
  installed key and refuse to casually regenerate one). The signing key is generated and installed as
  the `NIX_CACHE_SIGNING_KEY` Actions secret; its public half,
  `punktfunk-cache-1:yhOJmHxzg6tzXpxSFzlYn6Pc6r0jHprsWqt8MZC654o=`, is pinned in `install.md` and
  `packaging/nix/README.md` and served by the cache at `/punktfunk-cache.pub` (the wizard compares the
  two and warns on mismatch). DNS for `nix.unom.io` is provisioned through `unom/infra`'s OpenTofu
  (`terraform/cloudflare/records.tf`, applied by `dns-cutover.yml`) — not a dashboard click.
  `inputs.punktfunk.inputs.nixpkgs.follows` defeats the cache entirely. Rejected: Gitea's package
  registry (no Nix type), storage.unom.io (home uplink, and S3 answers 403 not 404 for a missing key,
  which nix treats as fatal).
- **Apple console-UI parity** (Swift, PunktfunkKit/PunktfunkShared): `LibraryCollation` ports
  `pf-console-ui`'s `collate.rs` (the desktop's eight tests by name; both read
  `clients/shared/library-collate-vectors.json`, new — desktop is the source of truth and regenerates
  it); `GameEntry.platform` (sent in `GameMeta` all along, dropped by `Codable`); `LibraryPlaceStack`,
  `CollectionsHandover.decide`, `LibraryGridCursor` (port of `GridShape`/`grid_step`/`grid_col_hint`,
  nine grid tests by name), `GridGeometry` (the grid owns its scroll offset — no trackpad wheel on the
  grid, a named trade); `ConsoleContract.swift` pins `ConsoleMotion` to the shared vectors'
  `motion_spring` (response 0.42, damping 0.88, slide 36, scales 0.985/0.96, reveal 0.4,
  interruptible; the v1 `$deprecated` note now names Android as the last v1 reader — and Android
  moved to the shared shell in this same release). Device keys `librarySort` / `libraryView` /
  `libraryCollections` / `libraryGroupBy` — presentation only, never in a profile. `PosterImage`
  decodes at the drawn size (`CGImageSourceCreateThumbnailAtIndex`). `HostCardView`'s primary action
  reverted to connect (`22fdea66` reverted; `swift test` 375/0). New dev hooks
  `PUNKTFUNK_FAKE_LIBRARY=<file.json>`, `PUNKTFUNK_SHOT_EDITING=<field>`, `PUNKTFUNK_SHOT_INTERACTIVE=1`
  (screenshot harness only).
- **New environment variables:** `PUNKTFUNK_PAD_SINK_VOLUME` (`=0` skips both pad-sink pins),
  `PUNKTFUNK_DUALSENSE_USBIP_GRACE_MS` (pad-arrival grace), `PUNKTFUNK_USBIP_TRACE` (byte-level
  USB/IP trace prefix, off by default), and the three Apple screenshot-harness hooks above.
  `PUNKTFUNK_STREAM_SINK` gained the `stream` value and is documented for the first time.
- **New packaging payload (Linux host, rpm/deb/arch; nix where noted):** `scripts/60-punktfunk.rules`
  (+2 sound rules), `scripts/60-punktfunk-dualsense.conf` (WirePlumber, also nix),
  `scripts/alsa-ucm2/…` (UCM drop-in, **not** nix). Bazzite sysext inherits all three from the RPMs.
- **Docs:** `AGENTS.md` + `docs/agents/` (issue tracker is Gitea via the `gitea` MCP server; the
  five triage labels; single-context domain docs). A host audio-source comment corrected
  (`pw_impl_node_set_driver` marks props changed but leaves the flush to the next info emission).
- **CI:** Nix publish job records `df` after the build as well as before.

### Verification status

Gates run on the release tree (this MacBook, rustc/rustfmt 1.96.0 per `rust-toolchain.toml`):
`cargo fmt --all --check` clean — **after** a whitespace-only commit on the release branch: two files
(`pf-console-ui/src/screens/controllers.rs`, `punktfunk-host/src/audio/linux/pad_card_volume.rs`)
had landed on main formatted differently from rustfmt 1.96.0, so `ci.yml`'s Format step was red on
the tip this is cut from; `cargo metadata --offline` ok with the `Cargo.lock` diff versions-only
(36/36 lines); `cargo test -p punktfunk-core` **272 passed** in the unit suite; the android.yml Play
notes gate run verbatim — 498/500 characters and not byte-identical to any prior release's; both
openapi copies `cmp` identical and unchanged since the tag; `include/punktfunk_core.h` regenerated
by the build and `git diff` clean against the tag.

⚠ **The C ABI harness (`tests/c_abi.rs`) did not run on this cut**: it links the staticlib with
`-lopus` and this machine has no libopus (`ld: library 'opus' not found`), which is an environment
gap, not a code fault. The header it exercises is byte-identical to v0.30.0's, where the harness
passed (261 + 1 + 8), and nothing in `punktfunk-core`'s C surface changed. The CI runner is its
first execution for this tag.

⚠ **Verified by reading only** — compiled nowhere available to the cutting host: the Windows runner
log redirect (`scripting-run.cmd`), the tray's `Option<u16>` port on Windows, and the sway half of
`topology: exclusive` (no live sway in the fleet, as with #283).

⚠ **Not verified on hardware by this cut**, named rather than left to be discovered: the null-sink
capture topology's on-glass validation (pw-top showing our sink at the top of its own group, 5 min
of loud audio at `delivered_pct=100 gaps=0` on a box where a hardware sink also runs) was still owed
when it landed; the 96 kbps speaker lane was judged on glass by ear only; and the Android
`ASurfaceControl` path was verified on one device (Nothing Phone 3) — the fallback presenter is
byte-for-byte the 0.30 one.

---

## v0.30.0

175 commits since v0.29.0 (131 non-merge).

Two large surfaces landed in this cycle and both are **additive**: a lossless PCM audio plane
alongside Opus, and per-client access grants on the trust record. Between them the C ABI moves
**20 → 24** in four steps, every one of them a new symbol — no existing function changed its
signature or its behaviour, and no `#[repr(C)]` struct grew a field. `WIRE_VERSION` stays **2**:
everything new rides the trailing-field append discipline the `Welcome` has used since v20, so an
older peer stops reading earlier and negotiates exactly what it always did. Every 0.29.x host,
client, driver and plugin keeps interoperating in both directions, with no re-pairing.

Three things need a hand rather than merely an update. The keyboard-layout fix needs a gamescope
rebuilt at `+pfhdr8`; two Windows hardening changes alter what the service will adopt out of
`%ProgramData%\punktfunk\host.env`; and two **defaults flipped** — the host now serves the lossless
audio plane unless told not to, and the "Show game library" client setting is gone along with the
field behind it.

### Versions

| | v0.29.0 | v0.30.0 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged — `Hello` and `Welcome` grew trailing fields older peers never read, and one new control message took a free type byte (below) |
| C ABI | 20 | **24** | four additive steps: 21 `connect_ex10`, 22 access, 23 `audio_plc`, 24 `connect_ex11` (below) |
| Rust edition | 2024 | **2024** | unchanged |
| MSRV (`rust-version`) | 1.85 | **1.85** | unchanged |
| Workspace crate dirs | 27 | **27** | unchanged |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3); `pf-driver-proto` shows no diff against the v0.29.0 tag |
| Windows virtual-gamepad channel | 3 | **3** | unchanged |
| Plugin index schema | 1 | **1** | unchanged |
| `api/openapi.json` | 0.28.0 | **0.29.0** | regenerated for diagnostics, client-log bundles, access grants and the game-lifetime fields; it keeps the stamp it was regenerated under. ⚠ The ungated `docs-site/public/openapi.json` copy had drifted (see below) and is re-synced byte-identical in this release commit |
| gamescope patch level (`+pfhdrN`) | 7 | **8** | patch 0010 puts the compiled keymap on the seat's stub keyboard (below) |
| `@punktfunk/host` (SDK) | 0.1.4 | **0.1.4** | unchanged |
| `@punktfunk/plugin-kit` | 0.4.1 | **0.4.2** | the Steam art scan learned the newer filenames and the flat layout |

### ⚠ Breaking changes

- **C ABI 20 → 24, addition only.** Four bumps landed in one cycle; each adds symbols and removes
  nothing. An embedder that compares `PUNKTFUNK_ABI_VERSION` at build time rebuilds against the new
  header and is done; one that adopts none of the new calls behaves exactly as it did on 20.
  - **v21 — `punktfunk_connect_ex10`**: `connect_ex9` plus `device_name`, the label an unpaired
    client knocks with. The C ABI had no such parameter, so every embedder took the OS default,
    which resolves through `COMPUTERNAME`/`HOSTNAME` — neither of which exists in an Apple GUI
    process, leaving every Mac, iPad, iPhone and Apple TV knocking as the literal "This device".
    `ex9` keeps its parameter list *and* its behaviour (it passes a null name, selecting that same
    default). The name rides `Hello::name`, which hosts have read since the pending list existed.
  - **v22 — the per-client access surface**: `punktfunk_connection_grants` and
    `punktfunk_connection_access_expires_in` read the session's live access state; 
    `punktfunk_connection_end_reject` reports the typed rejection a mid-session close carried
    (`PUNKTFUNK_STATUS_REJECTED_*`, `0` = none), because `end_reason` can only file an
    access-expiry close under `HOST_ERROR` and that is the wrong sentence for "your access
    expired". The host enforces grants either way — an embedder that never adopts these simply
    lacks the courtesy UX.
  - **v23 — `punktfunk_connection_audio_plc`**: one frame of libopus packet-loss concealment
    synthesized from the connection's *own* decoder state, for an embedder whose playout ring is
    draining because nothing is arriving. A second decoder is not a substitute: PLC extrapolates
    from the last decoded frame, so a fresh instance conceals from empty state. Frames it returns
    carry `seq` and `pts_ns` of `0` — concealed audio was never on the wire and must not reach an
    A/V-sync observation.
  - **v24 — the lossless plane's client surface**: `punktfunk_connect_ex11` asks for a sample rate
    and depth; `punktfunk_connection_audio_sample_rate` / `punktfunk_connection_audio_bits` report
    what the host actually **resolved**, which may be lower;
    `punktfunk_connection_next_audio_pcm` decodes both planes behind one call.
    ⚠ **The format is read through accessors, never through a struct field, and the distinction has
    teeth here.** The natural home for a rate is a field on `PunktfunkAudioPcm`, which is
    `#[repr(C)]` with no `struct_size` guard and is allocated **by value** by every C embedder —
    growing it would change its layout under all of them at once. `PunktfunkStats` is in the same
    position. This is the rule v18 set with `next_rumble_cmd2`.
    `PUNKTFUNK_AUDIO_SAMPLE_RATE_HZ` keeps its value and its meaning as the **default/legacy** rate,
    so a ring sized from it stays correct for every session that resolves to Opus — which is every
    session an ABI-23 embedder can ask for.
- **`PUNKTFUNK_AUDIO_HIRES` on the host defaults ON — explicit-off grammar (operator-visible).** The
  host's half of the lossless gate was an operator opt-in; it is now `0`/`false`/`off`/`no` to
  refuse and anything else to allow, the same shape as `PUNKTFUNK_444`, `PUNKTFUNK_CHACHA20` and
  `PUNKTFUNK_10BIT`. The field stops being the one `Option<bool>` in `pf-host-config` read as
  `unwrap_or(false)` and becomes a plain `bool`. The old default rested on "this spends bandwidth
  the host's owner never agreed to" — every clause of which is still true, except that the operator
  is not who spends it: the *client's* menu choice is, and that still ships OFF. What actually
  protects a link is mechanical rather than consent-based (the capture path must honestly deliver
  the rate, the cost must fit a quarter of the session's video bitrate, and a frame must fit a
  datagram at that channel count), so the operator gate was not keeping modest links safe — it was
  keeping the feature unreachable. **No wire or ABI movement:** `HOST_CAP_AUDIO_HIRES` is set only
  when a session actually resolved to PCM, so it remains a statement about that session's wire
  rather than a capability advert, and an ordinary session without `CLIENT_CAP_AUDIO_HIRES` is
  byte-identical to before. ⚠ The Android comments warning that sending `48000/16` as a stand-in for
  "default" silently opts a user into PCM described the blast radius as "any host with
  `PUNKTFUNK_AUDIO_HIRES=1`" — that is now every host that has not deliberately opted out, so the
  `0`/`0` sentinel is load-bearing in a way it was not before.
- **`trust::Settings::library_enabled` is removed, and the "Show game library" row with it.** The
  console never read the field — its row rendered a value and flipped a bool while every library
  affordance in that shell was gated on pairing alone, so it showed "Game library · Off" and handed
  you the library on Y anyway. What the flag *actually* gated was GTK and WinUI, which hid
  "Browse library…" unless it was on, with a stored default of `false`. Deleting only the row would
  therefore have left every user who never found the switch with a hidden library in the desktop
  apps. **Migration is by construction, not by luck:** `Settings` is `#[serde(default)]` with a
  `#[serde(flatten)] extra` map, so a stored `"library_enabled": false` parses into `extra`,
  round-trips untouched and is ignored — everyone who had it off now has the library, and a
  downgraded binary still finds its old value under the same key. A test pins that contract. At the
  four GTK/WinUI menu sites the flag is replaced by the **pairing** predicate rather than dropped
  for an unconditional item: the library fetch authenticates with the paired identity, and GTK's
  saved cards include trusted-but-unpaired hosts, so an unconditional entry would promote a latent
  "fetch that cannot authenticate" into the default experience. Apple and Android keep their own
  toggles for now, deliberately — both already default TRUE, so nobody there loses anything.
- **Windows: `host.env` keys are now allow-listed into the service environment (operator-visible).**
  `load_host_env` used to import **every** key of `%ProgramData%\punktfunk\host.env` into the
  LocalSystem service's own environment. `%ProgramData%` lets `BUILTIN\Users` pre-create the
  directory, so an unprivileged user could plant `host.env` before install; a planted `SystemRoot`
  then redirected the absolute `icacls.exe`/`powershell.exe` paths the host builds from it — code
  execution as SYSTEM. Only the `PUNKTFUNK_*` and `RUST_LOG` keys the child already allow-lists at
  the spawn boundary are imported now. **An operator who kept unrelated keys in `host.env` will find
  they no longer reach the service.**
- **Windows: a non-admin-owned `host.env` or `web-password` is no longer trusted (operator-visible).**
  A `host.env` whose owner SID is not privileged is renamed aside and the default written over it; a
  non-admin-owned console-password file is rotated to a fresh random instead of being kept as an
  "upgrade". A file from a prior privileged install is Administrators-owned and is kept untouched.
  If you hand-authored either file as a normal user, re-create it from an elevated prompt.
- **gamescope must be rebuilt at `+pfhdr8` for the keyboard-layout fix (packager-visible).** The
  host hands the session `XKB_DEFAULT_*` on all four gamescope launch paths, gated behind a
  `+pfhdr8` probe — an older binary gets a warning naming the reason rather than a silent
  US keymap. The patch series applies `git am`-clean at the pinned `5fb8dce4`.
- **GameStream's UDP/ENet media plane now binds to the launch owner.** The Moonlight-compat video
  and audio endpoints used to bind to the first datagram from anyone, and any ENet peer could hold a
  connection (pinning per-peer reassembly memory). Source IPs that are not the launch owner's are
  now discarded until the 10 s learn budget is spent, and non-owner datagrams are dropped before
  ENet allocates per-peer state. GameStream is runtime opt-in and off in the shipped unit.
- **Re-pairing can no longer escalate access.** `TrustStore::add()` used to *replace* the record; it
  is name-only on re-pair now, so a device that was approved as a guest cannot re-pair its way to
  full control. Records with no grants recorded — every record written before this release — read as
  full and permanent, so nothing changes for an existing install.

### The lossless audio plane: PCM on `0xD3`, alongside Opus

Opus is 48 kHz *by construction* (`opus_encoder_create` rejects 96 000), so hi-res was never a
constant to raise. A second audio plane carries interleaved little-endian PCM under
`AUDIO_CODEC_PCM` (`0xD3`); `AUDIO_CODEC_OPUS` (`0xC9`) and its `0xD2` redundancy are untouched and
remain the default for every session that does not ask.

- **Rates and depths.** `audio::pcm::rate_is_supported` admits 44 100, 48 000, 88 200, 96 000 and
  176 400 Hz, at 16 or 24 bits, including hi-res surround. The 44.1 kHz family took a fix to reach:
  the rate was divided before it was multiplied, and `rate_hz / 1_000_000` is 0 for every rate below
  a megahertz.
- **Frames are sized from RAW worst case, not from a coded estimate.** A datagram over the path MTU
  is not sent at all and this plane is never fragmented. The real ladder at default MTU is
  48/16 → 5 ms, 48/24 → 4 ms, 96/16 → 3 ms, **96/24 → 2 ms**. ⚠ A frame carries a whole number of
  samples *per channel*, so `frame_us` is a **label, not a duration** — at 44 100 Hz a nominal 5 ms
  frame is 220 samples per channel, which is 4 988 662 ns. Size from `samples_per_frame`; take time
  from `frame_duration_ns`, never from `frame_us`.
- **FLAC was planned and deliberately not shipped.** It buys nothing structural here: its worst case
  is a VERBATIM subframe (raw + header), so it gets the same frame duration, packet rate and
  send-buffer sizing and saves *average* bytes only — while this plane rides outside the ABR loop
  and is therefore provisioned for peak. The host also quantises f32 → 24-bit without dither, so the
  low bits barely compress; at 24 bits a lossless coder saves least. `AUDIO_CODEC_FLAC_RESERVED = 1`
  keeps the numbering so FLAC stays purely additive later.
- **Capability bits.** `HOST_CAP_AUDIO_HIRES = 0x80` and `CLIENT_CAP_AUDIO_HIRES = 0x10`.
  ⚠ **`0x80` was the LAST free `host_caps` bit** — `video_caps` was already full, so the next host
  capability needs a second byte and an ABI bump. ⚠ `Hello`'s post-HDR tail is capped at 27 bytes
  (with no HDR block the decoder disambiguates by remaining length, so a 28-byte tail is misread
  *as* an HDR block); **8 of those 27 are now spent**.
- **The host serves it by default; the client's menu choice is the opt-in.** See the breaking-changes
  entry above — `PUNKTFUNK_AUDIO_HIRES=0` is now the refusal, and the decline log names the opt-out
  and the value it must have rather than sending people looking for something to enable.
- **Cost and the affordability rule.** The plane costs **1.4–8.5 Mbps** in stereo — up to **33.9** for
  176.4 kHz/24-bit 7.1 — against Opus's 256 kbps, and it rides QUIC datagrams **outside the ABR
  loop**, off the top of the link where ABR can neither see it nor claw it back. A session therefore
  gets it only if the cost fits **a quarter of that session's video bitrate**: a 5 Mbps session can
  afford no rung at all. On game content it is very unlikely to be *audible* — 256 kbps Opus is
  already effectively transparent and nothing above 24 kHz is hearable — so the win is
  **bit-exactness**: no lossy stage anywhere, and no resample for a host whose interface genuinely
  runs at 96 kHz.
- **The host's gate, and what it can and cannot promise.** Every decline lands back on
  Opus at 48 kHz. ⚠ The gate checks format validity, not delivery: on Linux, PipeWire ships
  `clock.allowed-rates = [ 48000 ]`, and a rate that is not listed is **resampled into the running
  graph rather than switched to** — so a sink declaring 96 kHz on a stock box gets resampled and the
  gate does not catch it. Add the rates to `clock.allowed-rates` to make the sink the graph driver.
  (`clock.rate` in the settings metadata is the *default*, not the live graph rate; read `pw-top`.)
  On Windows the decline is floored at 48 kHz so only a hi-res request can lose — declining whenever
  `engine_hz < requested` would open capture at 44 100 on an ordinary endpoint, which libopus
  rejects, regressing the shipped Opus path.
- **Client-side gating happens before the connect, because that is the last point at which declining
  is free.** Windows reads the render endpoint's own mix format and withholds the capability bit
  when the engine cannot carry the rate (`autoconvert` would otherwise silently downsample a 96 kHz
  stream on arrival while every log line agreed the session was hi-res). Android probes an AAudio
  stream at the requested rate, reads back what was granted and closes it without starting it —
  AAudio never substitutes a rate, so a 48 kHz rung under a 96 kHz session could only ever produce
  2× playback. The ladder is 96 → 48-keeping-depth → the legacy pair.
- ⚠ **48 kHz/16-bit lossless is unreachable through the parameter pair** — that request is
  byte-identical to a legacy one, and the capability is derived from "asked for a non-default
  format". 48/24 is the real bottom rung.
- **Settings vocabulary is shared across all four clients** under the key `audio_format`, so a
  profile round-trips between a phone, a TV and the desktop. ⚠ **The menus are not all the same
  length.** Apple and Android list the full ladder — `opus`, `lossless441`, `lossless48`,
  `lossless882`, `lossless96`, `lossless1764` — while `pf_client_core::AUDIO_FORMATS` (Linux,
  Windows and the Gaming Mode console) lists only `opus`, `lossless48`, `lossless96`. A value a
  build does not recognise resolves to Opus rather than refusing the connect, which is what makes
  the shorter list safe. On the desktop clients `PUNKTFUNK_AUDIO_HIRES` overrides the setting in
  **both** directions with a richer grammar (`1`, a bare rate, `<rate>/<bits>`, or `0`); an
  unparseable value warns and is ignored rather than meaning "off". ⚠ Note the two spellings of the
  same variable name: on a box that is both host and client one line configures both halves, and
  `0` is the one value that means *off* to each.
- ⚠ **`pf-client-core` filters a surround request out before it reaches the wire**, so the desktop
  clients and the Gaming Mode console stay on Opus whenever `audio_channels != 2` — even though the
  host's `channels != 2` decline is gone and the arithmetic supports surround (at 48 kHz both 5.1
  and 7.1 fit a 1 ms frame). Apple reaches the host through `connect_ex11` directly and is not
  subject to it. Delete the arm when the client-side filter learns the ladder, not when the host
  rule changes — the comment at the site says so.

### Per-client access: grants on the trust record

Six bits, an exhaustive classifier and a `Welcome` advert. `PUNKTFUNK_GRANT_GAMEPAD`, `_POINTER`,
`_KEYBOARD`, `_CLIPBOARD`, `_MIC`, `_LAUNCH`; `PUNKTFUNK_GRANT_ALL` is their union and
`PUNKTFUNK_GRANT_RESERVED` its complement. Presets: `_PRESET_FULL` (= ALL), `_PRESET_CONTROLLER_ONLY`
(= GAMEPAD), `_PRESET_VIEW_ONLY` (= 0).

- **Storage is back-compatible by absence.** The grant mask and `expires_unix` are optional serde
  fields on `PairedClient` in `punktfunk1-paired.json`; absent means full and permanent.
- **Enforcement is default-deny and happens at setup**, not per-event: no uinput pads are created,
  no mic is attached, no clipboard coordinator is started for a device that lacks the bit. Launch is
  refused at the handshake with a typed reject. QoS messages (reconfigure, bitrate) are deliberately
  ungoverned.
- **Expiry is wall-clock**, warned at T−5m and T−1m over a new `AccessUpdate` control message, and
  closed with `RejectReason::AccessExpired` (`ACCESS_EXPIRED_CLOSE_CODE = 0x69`). An expired device
  falls into the knock path for one-click re-grant.
- **The mgmt API carries grants in its payloads**, with `PATCH` to change them live; a per-fingerprint
  watch pushes the edit into any running session. ⚠ `PATCH` 404s on a Moonlight client with no
  registry record — the console shows those as "Full (ungoverned)" until the mgmt upsert lands.
- **Moonlight/GameStream**: an absent registry record is ungoverned `GRANT_ALL`; an existing record
  governs launch, input and the clock.
- ⚠ **Three limits are documented rather than papered over** (`docs-site` → Access levels):
  shared-desktop visibility is not isolation, Moonlight rows are ungoverned until they have a
  record, and older clients are *enforced* but get none of the chrome (no chip, no countdown).

### The presentation clock: play frames out on the source's cadence

Every client used to present a frame the moment it was decoded, so the transport's jitter landed on
the glass 1:1. `CadenceClock` estimates the offset between the source clock and the present clock
and returns a due time on the source's own timeline plus a cushion sized to the measured jitter.

- It is **type-2** (offset *and* per-frame rate) because two free-running crystals produce a ramp and
  a proportional-only loop lags a ramp forever. Fixed-point `i64` throughout, so it runs identically
  on every client and in the offline harness.
- **It smooths the OFFSET, never the timestamps.** Genuine variation in the source's own cadence — a
  variable-rate renderer, an irregular capture tick — passes straight through. Anything that made due
  times more evenly spaced than the source would be a bug, and `preserves_source_cadence` is the test
  that says so.
- **Domain-agnostic by construction**: a constant offset between clock domains is absorbed by the
  offset estimator, so each client feeds `ready_ns` and reads `due_ns` in one domain with no
  conversion anywhere in the path.
- **VRR stops being merely not-worse.** Where the presenter has *measured* variable refresh (the
  existing `CadenceProbe` verdict, not a capability bit), the snap is skipped and the frame is
  presented at its due time under `free_running()` tuning. `Unknown` reverts to snapping — the
  absence of a measurement is not a measurement.
- **The host half is the prerequisite, not the alternative.** All three Linux publish sites stamped
  `pts_ns` with `SystemTime::now()` inside *our* PipeWire process callback — the instant the buffer
  was delivered to us, not the instant the compositor produced it. Source-timestamp playout would
  have faithfully reproduced that jitter. The compositor's `spa_meta_header.pts` is now rebased into
  the wire's realtime domain from a re-sampled clock pair, with a per-frame plausibility gate falling
  back to the delivery stamp (counted, never silent) beyond 50 ms. `PUNKTFUNK_CAPTURE_HDR_PTS=0`
  restores delivery stamps.
- ⚠ **`host_us` and `e2e` now read HIGHER** by the delivery delay that was previously not counted.
  The numbers moved because they got truer.
- ⚠ One-present-per-slot is now `PresentGate`'s job. Where present timing is live that is strictly
  better; where it is unavailable the gate is inert and a faster-than-panel stream can submit two
  presents into one vblank. That was already `latency`'s behaviour on those boxes but it is new for
  `smooth`.

### Audio robustness: droughts, holes and what the counters actually mean

- **Drought concealment on the decode thread.** The decode path already concealed a *seq gap*, but
  that only fires when a later packet arrives to reveal it. When the wire simply goes quiet the ring
  drains, the callback runs short, and the de-jitter policy de-primes and re-primes a whole target's
  worth of fresh silence — an artifact far longer than the audio actually missing. `DroughtConceal`
  is bounded by `JitterTuning::plc_max_ms()`, **twice the preset's own de-prime fuse, derived rather
  than added as a fifth field** so it cannot drift from the thing it protects. Denominated in
  **time**, never in frames or callbacks — that is the recorded lesson from this very fuse, where a
  count gave an iPad a third of a Mac's slack. `plc_ms=` joins the 10 s playback line at all three
  Rust sites; Apple reaches the same behaviour through ABI 23's `audio_plc`.
- **A capture hole cost more than itself.** `audio_thread` blocked in `next_chunk` for the whole
  duration of a hole and nothing left the host. The loop is deadline-driven now
  (`next_chunk_within`) and covers a hole with silence frames on the existing pacer schedule,
  continuous in `seq` and `pts`, for up to 500 ms. Past that the host is not glitching, it is quiet.
  The capture counters deliberately measure *upstream* of the infill, so infill can never make that
  line look healthy.
- **`delivered_pct` could not distinguish an outage from starvation.** `CaptureStats` gains
  `gaps`/`max_gap_ms`/`missed_dequeues`, and `pauses`/`paused_ms` beside the percentage they explain
  — a 16.2 s `Paused` span scored `gaps=0` correctly and hid the outage inside `delivered_pct`,
  because the reporting window is flushed from the process callback and stretches by exactly the
  time we were absent. The negotiated quantum now tracks what the graph actually hands over (a new
  size must survive three callbacks before it is believed) instead of latching the first callback.
- **`audio egress` is a new 30 s line** — `sent`/`infilled`/`late`/`max_late_ms`/`max_spacing_ms`/
  `reanchors`. The send path had no periodic metric of any kind, which left "the host paces audio
  badly" unfalsifiable. Note `reanchors` in particular: the pacer forgives accumulated debt
  silently, and that is precisely the event that leaves no trace and then gets blamed on the network.
- **The minted sink sets `session.suspend-timeout-seconds=0`** — deliberately *not*
  `node.always-process`, which would keep the node scheduled with nothing connected and run its
  callback 200 times a second on a host sitting between sessions.
- **The capture phase lock stops flapping.** Engagement now needs five consecutive coherent reports,
  each incoherent cycle backs off longer than the last (10 ticks doubling to 320), and a host that
  has torn down an engaged grid eight times parks the lock for the session. Only a disengage that
  tore down an *engaged* grid counts toward the fuse, so a launch-time shader storm cannot fuse a
  host that then locks perfectly for hours. The disengage line gains `coherence_milli`.

### ABR: the behind-cadence deadline was the negotiated refresh

A frame's encode work was scored against the negotiated interval (8.33 ms at 120 Hz), but a frame's
real budget is the arrival of the next frame that actually exists — a 60 fps source gives every
frame twice that. An encoder keeping up with every real frame could be marked behind on most of
them, latch `behind_score` past `DEPTH_DEGRADE` and refuse every climb the client asked for. The
budget is now the **observed source-delivery period**: an EMA over real frames' arrival spacing
(repeats excluded), clamped to `[interval, 4 × interval]` so a source at or above the negotiated
rate keeps today's deadline bit-for-bit and a hitchy source cannot disarm the detector. Every
`cadence_degraded` transition now logs `behind_score`, `escalated` and budget/interval/observed
period, rate-limited to one line per 5 s with suppressed flips counted; the climb-refusal line
carries the live `behind_score`.

### ABR, client side: a host-local rebuild is no longer read as congestion

A Windows exclusive-topology eviction rebuilds the capture ring and encoder in place — a few hundred
milliseconds, entirely host-local, not one packet lost. The client decides on 750 ms windows, so a
window straddling that rebuild sees almost no stream: the 0.29 field log shows 401 ms of rebuild
producing `actual_kbps=390` against a 20 000 target with `loss_ppm=0`, read as congestion, costing
×0.7 plus slow start and three minutes at ~15 Mbps on a link that never dropped a packet. Three
independent changes:

- **`PipelineGap` — a new control message, type byte `10` (`0x0A`).** It extends the video/rate-control
  block (`0x01`–`0x09`) it belongs to, because its only consumer is the same ABR controller
  `LossReport`, `SetBitrate` and `BitrateChanged` already feed — deliberately **not** in the `0x30`
  clock block, since it carries a *duration* precisely so that no clock domain is involved. The host
  is the one party that knows, and it now says so; the client already knew how to throw a window
  away (`discard_abr_window` feeds the controller nothing, sends no `LossReport` so a bogus window
  cannot spike the host's adaptive FEC, and closes the standing-latency detector as not-loss-free).
  That path had exactly one cause — the tail of the client's own speed test — and this is its second.
  ⚠ The header gains `PUNKTFUNK_MSG_PIPELINE_GAP` as a `#define`; a message-type constant is additive
  and takes a free type byte, so neither `PUNKTFUNK_ABI_VERSION` nor `WIRE_VERSION` moves.
- **A starved window cannot report the encoder as slow.** `encode_us` is a per-AU host measurement
  averaged over the window; when almost no AUs flowed, the mean is taken over the handful that
  straddled whatever interrupted them and carries that interruption rather than the cost of encoding
  at this rate. It is not a measurement, so it is now withheld entirely when the window is `STARVED`
  — passed as **absent** rather than ignored, so it cannot teach the rolling-minimum baseline either.
  Deliberately narrow: loss, a flush and a dropped frame describe what reached the *client* and mean
  the same thing however little flowed, so those still back off on one window as
  `STARVED_DELIVERY_DIV`'s own comment requires. Only the host-encode signal is withheld, because
  only it is measured over AUs that did not exist.
- **The learned climb ceiling is bounded by what the stream could plausibly use.** The ceiling was
  pure link capacity (`delivered_kbps * 0.7`) with no term for resolution, frame rate, codec or bit
  depth, and the utilization gate cannot supply one — a hardware encoder in CBR mode genuinely fills
  whatever target it is handed, so the field log shows 99 % utilization the whole way up. On a
  gigabit LAN the probe measured 939 Mbps and the session walked to **657 Mbps for 1440p120** — 1.49
  bits per pixel — in 37 seconds, taking client decode latency from 0.78 ms to 10 ms.
  `stream_ceiling_kbps` computes a bound from pixel rate and a bits-per-pixel allowance varying by
  codec generation, bit depth and chroma; `set_ceiling` holds the measured link ceiling to it and
  logs both numbers whenever it binds. Deliberately generous — a bound on the absurd, not a quality
  opinion: 1440p120 HEVC Main10 lands at ~414 Mbps while 1080p60 HEVC keeps ~93 Mbps. It binds only
  what the probe **learns**: a negotiated start rate is a number the host resolved on purpose, and an
  explicit bitrate and every PyroWave session are outside the controller entirely.

### AV1 never decoded on AMD, because the bit reader refused 32-bit reads

Every AV1 session on an AMD host died after ~287 frames and silently fell back to H.265 — the client
log named it on the first access unit ("AV1 parse: more than 31 (32) bits were requested") and then
"No sequence header parsed yet" for every AU after, because the sequence header never parsed and each
new keyframe re-hit the same wall. The vendored cros-codecs `BitReader` refused any read wider than
31 bits "because that would break the `read_bits_signed()` function" — true of the signed path's
`i32` accumulator, and misplaced: AV1 needs 32 bits in five places (`timing_info`'s
`num_units_in_display_tick` and `time_scale`, `decoder_model_info`'s `num_units_in_decoding_tick`,
and the variable-width buffer-delay and `buffer_removal_time` fields). **AMF sets
`timing_info_present_flag`; NVENC does not** — which is why the rung's own evidence string ("one
vendor, no soak") described a codec that had never once decoded on AMD.

Relaxing the guard alone would have been worse than the bug; three edits are required together: the
trailing mask is `u32::MAX` at 32 (`1u32 << 32` overflows — a debug panic, and in release a mask of
zero, i.e. a silent `0`); the byte cursor is advanced before the accumulation loop when it sits at
zero remaining bits (at ≤31 bits the mask discarded those bits, so it was invisible); and
`read_bits_signed` keeps its own `> 31` guard, which is the limit the original comment was actually
protecting. That last guard made a latent panic reachable by test — the sign extension
`-1 ^ ((1 << num_bits) - 1)` overflows at `num_bits == 31`, rewritten as `-1i32 << num_bits`.
**Blast radius is provably AV1-only**: neither H.264 nor H.265 has a read wider than 31 bits, literal
or variable, and every dynamic-width call site in the vendored tree is in the AV1 parser. The 52
upstream conformance tests still pass unchanged. Recorded as PROVENANCE deviation 8; owed upstream as
a cros-codecs issue.

### Game lifetime: a lease nothing is watching now says so

Quitting a game mid-stream could leave the session up and the console showing it as "running"
forever, with no setting that made any difference — because a lease with nothing to recognise its
game by set its **own** state to `Running`, on the reasoning that the host had just launched it. That
made three situations indistinguishable: a game being watched, a game that quit and was never
noticed, and a game the host cannot see at all. `session_on_game_exit` can never fire for a lease
nothing is watching, which is why no setting helped.

- **`GameState::Untracked`** — surfaced in `/status`, the console card and the tray label. Keyed on
  "is anything watching this" rather than on the lease kind, so a nested gamescope lease (whose exit
  the capture loop catches) still correctly reads `running`.
- **`LeaseRequest::spawned`** carries the pid Windows already knew and discarded, pinned to its start
  time by a new `Scanner::resolve` so a recycled pid cannot impersonate it. Windows never holds a
  `Child`, so a title whose provider published no detect hint had *nothing* identifying it: its exit
  went unseen and `POST /game/end` had no pid to signal, while the same title on Linux was fully
  tracked through its child. The same fix closes a second defect — a launch that adopted nothing
  answered `Unknown`, fell back to the 90-second in-flight window, and past it started a **second**
  copy, which is why "click the game that is already running" resumed on Linux and relaunched on
  Windows.
- **`game_on_new_launch` (`keep`|`end`, default `keep`)** — close this client's previous game before
  starting a different one. Its own axis rather than a fourth `game_on_session_end` value: wanting a
  game to survive a disconnect says nothing about wanting it kept when you deliberately pick another.
  Four safety rules, made pure and unit-tested — never another client's game, never one the player
  started themselves, never the title being launched, and never a record whose liveness is merely
  `Unknown`. Scoped to launches this host performed itself (`crate::launchreg`).

### The virtual DualSense can arrive as a real USB device (opt-in)

A game that drives DualSense haptics pairs "my controller" with "my controller's speaker" by Windows
`ContainerId`, and wine derives that by walking the HID device through udev up to a `usb_device`
parent. Our pad is uhid, so its sysfs chain has no USB ancestor anywhere: winebus logs "Failed to get
parent device" and every endpoint registers as `GUID_NULL`. Measured against Spider-Man Remastered
under GE-Proton11-5, the game resolves both endpoints, reads their `FriendlyName` and
`PhysicalSpeakers`, and then declines to open either. The same missing fact blocks GE's other route:
its haptic path enumerates real **ALSA cards** (`snd_card_next` → `snd_ctl_pcm_next_device` →
`snd_pcm_open` demanding 48 kHz / S16 / 4 channels), and minted PipeWire nodes are not ALSA cards
however faithfully their proplist impersonates one — in the field log that scan never ran at all.

So `PUNKTFUNK_DUALSENSE_USBIP=1` presents the pad over `vhci_hcd`, reproducing the hardware's own
4-interface composite layout from an lsusb capture of a wired `054c:0ce6` — audio control, audio
streaming out (isochronous, S16LE 4ch 48 kHz: haptics + speaker), audio streaming in (the headset
mic), and HID. Because interfaces 0–2 are a genuine UAC 1.0 device, `snd-usb-audio` binds them and
mints a real ALSA card, and PipeWire's ALSA monitor builds the `HiFi__Speaker__sink` /
`HiFi__SpeakerHaptic__sink` nodes itself from the distro's DualSense UCM — the node graph stops being
impersonated. The transport rides the ladder `steam_controller` already established (usbip → uhid,
degrading on failure) and reuses the udev grant packaging already ships for the virtual Deck, so it
needs no new privilege, module or packaging. The descriptor set is pinned by test against the
hardware's published `wTotalLength` of `0x00E3`.

Pad-audio capture follows the pad: with the usbip pad the host mints nothing, so capture moves to the
**isochronous OUT endpoint** — the same point a physical pad's samples reach, which is what makes any
route a game takes land in one place. The `0xD1` wire path downstream is untouched; only the source
moves. The two capture modes are mutually exclusive by construction and the choice is read from the
transport flag rather than from whether a stream happens to have been published, or the race between
pad arrival and the streamer thread starting would decide it. `pad-usbip-test` is the on-glass gate
with no client and no game involved.

⚠ **Opt-in while it awaits on-glass verification** — it changes the pad's whole kernel presentation,
including superseding the minted pad-audio sinks with a real card. The uhid pad remains the
long-validated default.

### Recovery anchors are now corroborated

`USER_FLAG_RECOVERY_ANCHOR` is the host asserting a fact about the *client's* decoder — "the picture
I coded this P-frame against is one you still hold, intact" — and the gate took it on faith, on the
first occurrence. The host derives that claim from bookkeeping that tracks what the client
**received**, not what it managed to **decode**, and those diverge exactly when the client had to
conceal. `pf-bitstream`'s planners now carry a per-picture clean bit (damage propagates down the
prediction chain), surfaced as `PicturePlan::references_clean` and carried on `DecodedVkFrame`; the
gate gains `AnchorEvidence` and refuses an anchor whose references the client can prove were damaged.
Refusing can only ever hold **longer** — the freeze stays up and the backstop fires on its original
deadline. Lanes with no local parser pass `Unavailable` and are bit-for-bit unchanged; all 29
pre-existing reanchor tests pass untouched. On the host side, the slot-family RFI backends no longer
pick an anchor over damage the client has already reported.

⚠ **The H.264 decoder reached parity here, having been the only one of the three that failed OPEN**:
a DPB slot with no bound image was traced and decoded anyway, `reference_count` was computed after
the held-slot loop so a dropped reference silently took an unrelated slot's picture, and there was no
`RecoveryLatch`. Both latches landed with HEVC and AV1 in August; H.264 predates them and was never
retro-fitted.

### New management API surface

- `GET /api/v1/diagnostics` and `POST /api/v1/diagnostics/refresh` — **admin lane only**, because
  these verdicts carry the host user's name, its group layout and device-node state. `inapplicable`
  is a first-class status rather than an absent row, so the console can answer "why isn't this check
  relevant here?" instead of silently hiding it. Probes stay in their owning crates
  (`pf-vdisplay`, `pf-inject`) and export plain verdict enums; the host does the mapping, so there is
  no reverse dependency. A check id the console has never heard of still renders, from the host's
  English `summary`/`impact`/`remedy.text` — that is what makes console N paired with host N+1
  survivable, and it is enforced as a test.
- `POST /api/v1/client-logs` — **the cert lane's first and only WRITE.** Paired mTLS devices may
  upload (write-only: no read of anything, not even their own bundle), while list/fetch/delete stay
  on the loopback bearer lane; the lane matrix pins all four rows. The gate uses `effective()`, so an
  expired guest's upload is refused (403) while any live-authorized device — including a view-only
  guest — may send. No input grant is required: uploading one's own diagnostics is not an input
  capability. Storage is a bounded file store under `<config-dir>/client-logs` (traversal-proof ids,
  newest 5 bundles per device, 1 MiB cap) and deliberately **not** the log ring, which a
  multi-thousand-line bundle would evict.
- `PUT /api/v1/library/provider/{p}` now runs the console-password gate. It had no BFF handler and
  fell through to the `/api/**` catch-all, which injects the full admin bearer — so a bare session
  cookie could plant a persistent `prep`/`launch.kind:command` entry without the password.
- **Schema additions**: `GameOnNewLaunch` (`keep`|`end`) with the `game_on_new_launch` settings field,
  and `untracked` joins the game-state enum described on `/status`. ⚠ A client reading the state enum
  must tolerate `untracked` — the console and the Rust/Android clients treat it and `grace` as "up",
  and only a confirmed `exited` as not.
- ⚠ **`docs-site/public/openapi.json` had drifted from `api/openapi.json` and is re-synced here.**
  The mgmt drift test compares the *live route table* against the generated document, not the two
  on-disk copies, and nothing else gates the docs-site mirror — so the game-lifetime regeneration
  updated `api/openapi.json` alone and CI stayed green while the published API reference silently
  described the previous release. The two are byte-identical again as of this commit. **Regenerating
  one means copying it to the other**; there is no build step that does it for you.

### New environment variables

| Variable | Effect |
|---|---|
| `PUNKTFUNK_AUDIO_HIRES` | Forces the lossless plane on or off, overriding the stored `audio_format` setting in both directions |
| `PUNKTFUNK_CAPTURE_HDR_PTS=0` | Puts Linux capture back on delivery stamps instead of the compositor's own `spa_meta_header.pts` |
| `PUNKTFUNK_SESSION_LAYOUT=0` | Disables `sync_session_keyboard_layout()` — the leg that points an adopted gamescope session's Xwayland at the box's configured layout |
| `PUNKTFUNK_PAD_AUDIO_PROFILE=0` | Stops the Linux client moving a DualSense card to Pro Audio when no four-channel node exists |
| `PUNKTFUNK_PAD_SINK_SPLIT_NAME` | Overrides or drops the host sink's `api.alsa.split.name` (GE-Proton's haptic-leg target) |
| `PUNKTFUNK_PAD_SPEAKER_PATH` / `_VOLUME` | Field levers for the DualSense speaker-enable packet — hex or decimal. The path byte is empirical (SDL's vendored `SDL_hidapi_ps5.c` pins the struct layout but never writes these fields) but is now settled on glass at a ~300× margin |
| `PUNKTFUNK_DUALSENSE_USBIP=1` | Presents the virtual DualSense over `vhci_hcd` as a real USB device with its own ALSA card, instead of uhid. Opt-in; changes the pad's whole kernel presentation |

### Keyboard layout: the wire is US-positional, and nothing carried the host's layout

The key wire is US-positional **by design** — a client sends the physical key, `vk_to_evdev` turns it
into an evdev code, and the session's keymap picks the character. So the contract is "host layout ==
the layout on the client's keyboard", and nothing was upholding it. `localectl set-x11-keymap de`
writes a file only Xorg reads, libxkbcommon's fallback chain stops at `XKB_DEFAULT_*`, and no session
manager sets them — so a properly configured German box compiled `evdev/pc105/us`, silently.
`pf_host_config::layout` now resolves what the box actually recorded (`XKB_DEFAULT_*`, then
`xorg.conf.d`, then vconsole's `XKBLAYOUT` — **never** vconsole's `KEYMAP`, whose names are console
names that do not map onto xkb's). The wlroots injector compiles its uploaded keymap from that, all
four gamescope launch paths export `XKB_DEFAULT_*`, and `sync_session_keyboard_layout()` covers the
adopted-autologin case where no launch-time decision applies.

gamescope patch **0010** is the other half: gamescope builds a keymap from `XKB_DEFAULT_*` but puts
it only on `keyboard_group`, while the *seat* carries `virtual_keyboard_device` — a stub whose own
comment says it exists "only to set the keymap" and which never gets one. `wlserver_keyboardfocus()`
rebinds that stub on every focus change, and the real group only reaches the seat from a libinput key
event, which a `--backend headless` session never has.

### Controller audio on Linux: what a real DualSense actually presents

We minted a single `AUX0..AUX3` node wearing the mono sink's `Speaker__sink` name. A game that
renders DS5 haptics writes a **positioned** FL/FR/RL/RR quad — the only public 4-channel surface a
physically connected pad publishes — so that write was position-remixed on arrival and the coil pair
folded away, measured at `peak_speaker=0.2441` with `peak_coils=0.0000`. Nothing errored, nothing
logged. The host now mints the three-node split a real pad presents (a 4-channel `AUX` parent, a
positioned `HiFi__SpeakerHaptic__sink`, a mono `HiFi__Speaker__sink`), all summed onto one hardware
quad. The profile suffix is `-00.HiFi__Speaker__sink`, because GE-Proton's
`is_dualsense_speaker_sink()` is a substring test for `Speaker__sink` and three separate behaviours
hang off it; `api.alsa.split.name` is published, without which `get_dualsense_haptic_target()`
returns NULL and the leg cannot engage at all. `device.vendor.id`/`device.product.id` gained the
`0x` prefix the specimen publishes — `strtoul(s,_,0)` reads a bare `054c` as octal, stops at the `c`
and yields 44.

The client half matched a sink by name signature alone and had the same folding problem. Correlation
now walks nodes **and** devices, requires `device.id` (which is also what keeps a Punktfunk host's own
minted pad sink out, since it carries the full DualSense identity on purpose), prefers unpositioned
AUX quads, and falls back to moving the card to Pro Audio for the session. The stream sets
`stream.dont-remix` with `AUX0..AUX3`, so channel *k* reaches channel *k* whatever the node
advertises. `punktfunk-session --pad-audio-test` prints every DualSense object in the graph, the node
it chose, and drives a tone into the coils — separating "the plane never arrived" from "it arrived
and the graph folded it away" with no host, no game and no pairing.

⚠ **Channel 1 of the DualSense's audio function is the headphone jack's right channel *and* the
built-in mono speaker**, and which one physically sounds is chosen by `ucAudioEnableBits` (report
byte 8). A pad powers up pointing at the jack, so with nothing plugged in the speaker pair went
nowhere — which is why haptics worked the instant routing was right and the speaker did not. A tier-A
slot with the speaker capability now sends a default speaker-enable packet; a game's own `AudioCtl`
still overrides it verbatim.

**The path byte is now settled, and was not when it first landed.** `0x20` was originally chosen from
a sweep that cleared the pad-mic noise floor by only ~5×, and was recorded as unsettled. A second
sweep on glass (DS5 wired to a Steam Deck client, 330 Hz into the speaker pair) swept the whole byte
at a **~300×** margin: `0x20` and `0x30` sound, `0x10`/`0x40`/`0x50` do not — so **bit 5 is the
speaker-path enable and bit 4 alone does nothing**. `0x20` stays the default over the marginally
louder `0x30`, which asserts a second path bit whose effect on the headphone leg was never measured.
Two properties the one-shot depends on were measured the same day: the setting persists (unchanged
40 s after a single write) and survives the pad's USB audio stream stopping and restarting, so it
needs no re-assertion when the renderer opens its output. Verified end to end — forcing the pad to
`0x00` dropped the pad mic to the `0.000000` floor and silenced the speaker; a fresh slot open
restored it.

### Bazzite: our virtual DualSense triggers an SELinux audit storm

The virtual DualSense/DualShock 4 binds `hid-playstation`, and Valve's `ds_inhibit`
(steamos-manager) reacts to every open/close of any such hidraw by walking `/proc/*/fd` — with no
VID/PID or virtual filtering. SELinux denies `steamos_manager_t` that walk at ~324 AVCs/sec, and
`setroubleshootd` amplifies it into a box-wide fork storm (267+ procs/sec, a core burned, RSS
climbing for 15+ minutes *after* the denials stop) that starves the stream to 0 fps and session
death. Punktfunk is the trigger, not the defect — but we ship the trigger.
`packaging/bazzite/punktfunk-ds-inhibit.cil` is a **dontaudit** drop-in, not an `allow`: granting
another vendor's daemon `sys_ptrace`/`dac_*` is not ours to do, and the scan failing quietly leaves
the pad uninhibited, which is what we want anyway. The RPM ships the source under
`/usr/share/punktfunk/selinux/` (the policy *store* is host state, so a sysext image can only carry
source) and it is inserted idempotently by `punktfunk-sysext post_merge`/`reapply` and best-effort by
the RPM `%post`, both keyed on the steamos-manager binary and the module name. ⚠ **Rename the `.cil`
if its rules ever change, or existing installs never converge.** `warn_if_ds_inhibit_storm` puts the
cause in *our* logs, because the AVC lines read `comm="tokio-rt-worker"` and look like us.

### udev: the virtual Steam Controller 2's hidraw node was root-only

`60-punktfunk.rules` grants hidraw access per product id and never listed the SC2 identities the host
mints — wired `28DE:1302` and Puck `28DE:1304`. For any other pad that would be a degradation; for
this one it is total, because no kernel driver claims the PID (mainline `hid-steam` stops at the
Deck) and the state reports ride a vendor collection, so there is no evdev node either. Steam is the
only consumer there is. Both identities are added in the same two forms the Deck uses (`KERNELS` for
the UHID shape, `ATTRS` for the usbip/gadget one). Single source of truth: every distro installs this
file, and the NixOS module takes it from the package.

### Other embedder-visible notes

- **Android**: `NativeBridge` and the audio path gained the format vocabulary; every Opus session was
  previously advertising the lossless capability, and the ABI doc told it to. The CI test filter is
  an allowlist and eleven classes sat outside it, including the audio HUD ones — all eleven pass,
  but nothing was gating them. They are listed explicitly now.
- **`pf-client-core` still filters a surround request out before it reaches the wire**, so the
  console's audio-format row stays gated under 5.1 even though the host's `channels != 2` decline is
  gone and the arithmetic supports surround at 48 kHz.
- **Reassembly memory is metered per block.** The reassembler's firewall counted only `FrameBuf::buf`
  bytes; `BlockState` (have-data + recovery vectors, both sized from attacker-declared header fields)
  was unmetered, so a slice-streamed frame could mint thousands of distinct-index blocks and commit
  multiple GB against a ~13 MB accounted figure. `block_state_bytes()`/`frame_cost()` now gate each
  new block on the same `IN_FLIGHT_BUF_FACTOR × max_frame_bytes` budget.
- **PyroWave**: a duplicate `block_index` with `payload_words == 0` spun the client decode thread at
  100 % CPU forever, inside FFI with no allocation and no timeout. The minimum-size check is hoisted
  into `push_packet` before `decode_packet` is consulted. Carried as vendored patch 0008.
- **Apple**: `HTTPResponse` overflowed `bodyStart + length` on a malicious `Content-Length`, and Swift
  integer overflow **traps** rather than throwing. `art_path_is_confined`'s UNC guard was a leading
  double-*backslash* string test, so `//server/share` and mixed forms slipped past and
  `canonicalize()` would coerce the SYSTEM host into outbound SMB auth.
- **CI**: `ci.yml`'s cargo-home cache shared an unnamespaced key with the signed release builds, so a
  fork PR could poison a release artifact through `registry/src`. The key is namespaced to
  `cargo-home-ci-`. The pinned `bun-windows-x64.zip` is now sha256-verified before it is staged and
  Authenticode-signed. ⚠ The Linux `curl|bash` sites (arch/rpm/deb + builder Dockerfiles) still need
  version+hash pinning.
- **`@punktfunk/plugin-kit` 0.4.2**: `library_capsule.jpg` is the newer name for the 2:3 cover and
  `header.jpg` the newer name for the header — measured against a real 779-app `librarycache`, 46
  appids carry only the former and 594 carry the latter, with **no appid carrying both spellings of
  either**. The `<appid>/<name>` layout with no hash directory is the **majority** (623 of 779), and
  `findLocalArtFile` walked only `<appid>/<hash>/<name>` and the oldest `<appid>_<name>`. Per-appid
  art found locally went portrait 25 → 328, hero 75 → 323, header 86 → 716, logo 66 → 295. ⚠ Each
  plugin vendors its own nested copy of the kit, so a plugin only picks this up when it is
  republished.

### Console UI: nothing was anti-aliased, and cover art sampled nearest

Reported from a Steam Deck, where a 1280×800 panel gives sub-pixel error nowhere to hide. Two
independent causes, both API defaults. **`SkPaint`'s default constructor sets `fAntiAlias = false`**,
and skia-safe's `Paint::new(colour, None)` is that constructor with a colour on it — so the terse,
natural spelling of a draw call silently produces hard-stepped edges. The crate had 70 paint sites
and 15 were anti-aliased, in an exact pattern: paints that got *mutated* for some other reason picked
up a `set_anti_alias(true)` on the way past, and every inline argument did not. The console drew
smooth 1 px rings on top of jagged fills, which looks worse than no ring — the smooth edge gives the
eye a reference for how wrong the fill is. Text was never affected, since glyph anti-aliasing is
`SkFont::Edging` and defaults on. Separately, **`Canvas::draw_image_rect` samples with
`SamplingOptions::default()`, which is `FilterMode::Nearest` with no mipmaps** — every 600×900 poster
minified into a ~180×270 Deck cell simply discarded rows and columns.

`theme::fill`, `stroke`, `shaded`, `shaded_stroke` and `layer` are now the only sanctioned paint
constructors and all 70 sites go through them; `theme::art_sampling` (linear + linear mipmaps) covers
the three image draws. ⚠ **`shaded` earns its place the hard way: Skia modulates a shader's output by
the PAINT's alpha**, so building a gradient paint from a transparent placeholder draws *nothing* —
not dimmer, absent. `Paint::default()` happened to be opaque black, which is the only reason the
console's gradients and the SkSL aurora never met the rule; building them transparently erased the
backdrop, badge, vignette and skeleton sheen at once, **and all 124 tests still passed**, because a
test that only renders a frame cannot tell a missing layer from a dark one. `shaded` is opaque by
construction so the trap cannot be set again. Three tests pin it, each verified to fail with the fix
backed out — including one that scans the crate's own source, because the aliased spelling is the
natural one and will be written again by whoever adds the next draw call.

### Other client and console notes

- **Client log bundles merge into the host log viewer** rather than sitting in a table beneath it.
  The Logs page had two axes fighting: `All | Host | Plugins` was a filter over one stream, while
  uploaded bundles were a different artifact kind stacked underneath. The format itself resolved it —
  the session ring layer writes `<ISO8601-Z> <LEVEL> <target> <msg>`, the same four fields as a host
  `LogEntry` and wall-clock stamped precisely so a bundle correlates with the host log. The source
  control is now multi-select chips over one merged pane with devices as peers of Host and Plugins,
  so "the client stalled at 12:03:47, what was the host doing?" is expressible for the first time.
  Bundles were previously undiscoverable twice over (the card returned null when empty, and when
  non-empty sat below a 65vh viewer) and could not be **read** in the console at all, only downloaded.
- **The library round-trip reached Android and the Gaming Mode console** — catalog cached per host
  and rendered immediately marked stale (keyed on the pinned fingerprint, so a new DHCP lease is
  still the same host), the host woken on library **entry** with the fetch retried across the boot
  window rather than at CONNECT which is too late to help, titles already up badged Resume and
  sorted first from `/status`, running-first ordering **within** each group only (`GridShape` lays
  launcher entries out as a prefix, so a running game jumping the fence would put the cursor
  arithmetic and the renderer on two different fields), and the grid returning to the last title
  *opened* rather than a pixel offset. Forgetting a host now drops its cached catalog. ⚠ **Item 1 of
  that set was reverted at review**: a host card's primary press stays "connect" on both carousels,
  because Y already opened the library there with a legend hint — the flip spent entrenched muscle
  memory on an affordance that already existed one press away.
- **A masked-color I-beam was forwarded as a fully transparent pointer** on Windows capture: `AND=1`
  plus a non-zero colour pixel is *invert*, not transparent, so treating it as a simple alpha mask
  dropped the text cursor and the client installed nothing over every text field.
- **The Apple client's settings captions went from 2 097 words to 973.** The rule applied throughout:
  one clause of what the setting does, one of what it costs. Numbers survive; rationale moves to the
  code comment above each caption. ⚠ The lossless gate riders are gone from all five rows and said
  **once** in the Audio section footer — that footer sentence is now the only thing keeping
  `audioFormatCaption` honest about the design's rule that the picker must never read as a promise of
  the *resolved* format, and its doc comment says so. `bitrateFooter` lost its speed-test sentence
  entirely rather than being shortened: that string is tvOS-only and directed Apple TV users to a
  context menu, and tvOS has no context menus.
- **An encode worker that died before the `Hello` landed reported `EPIPE`** instead of naming the
  handshake, and `pf-win-display`'s EDID unlock logged its expected no-op as a warning.
- **Flatpak CI: 21 minutes → 9.** The job reinstalled its whole toolchain every run (now a baked
  builder image), pulled 3.84 GB to publish 28 MB, and — the real defect — its "offline" build step
  **re-fetched every git source**, so one upstream 503 killed the build.

### Verification status

Gates green on the release tree: `cargo fmt --all --check`, `cargo metadata --locked`,
`cargo test -p punktfunk-core` including the C ABI harness, the whatsnew character and uniqueness
gates, and the release-notes voice scan.

⚠ **Not verified on hardware by this cut**, and named rather than left to be discovered: the 96 kHz
lossless path on Apple and Android devices (§13.2's on-glass tone check passed host → Mac at
96 kHz/24-bit, but the mobile legs gate on hardware); the on-glass guest rehearsal for per-client
access and a real-Moonlight run; a BLE-paired Steam Controller 2 on Android; and the diagnostics page
against the real `.181` box in all three vhci states.

⚠ **Verified by reading only** — these compile nowhere available to the cutting host, so a CI runner
is their first compiler: the WinUI half of the library-toggle removal (`punktfunk-client-windows` is
`cfg(windows)`), and the Apple library-browse pin gate.

⚠ **`PUNKTFUNK_DUALSENSE_USBIP` is opt-in precisely because it is unverified on glass.** It changes
the pad's whole kernel presentation and supersedes the minted pad-audio sinks with a real ALSA card;
`pad-usbip-test` exists to be the gate that closes this, and it has not been run against real
hardware. The uhid pad remains the default and is unaffected.

---

## v0.29.0

53 commits since v0.28.1 (36 non-merge).

The headline contract change is one **additive** C ABI bump: the host now tells the client, in-band,
where its management API lives, and the connection grew an accessor for it. The wire protocol, the
driver protocol and the plugin contract do not move; every 0.28.x host, client, driver and plugin
keeps interoperating with 0.29.0 in both directions, with no re-pairing. The one thing that needs an
operator's hand is on Windows: the MSIX package identity changed with the move to a publicly
trusted signing certificate, so that install path needs a one-time uninstall + reinstall.

### Versions

| | v0.28.1 | v0.29.0 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged — `Welcome` grew a trailing field older peers never read (below) |
| C ABI | 19 | **20** | one symbol added: `punktfunk_connection_mgmt_port` (below) |
| Rust edition | 2024 | **2024** | unchanged |
| MSRV (`rust-version`) | 1.85 | **1.85** | unchanged |
| Workspace crate dirs | 27 | **27** | unchanged |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3); `pf-driver-proto` shows no diff against the v0.28.1 tag |
| Windows virtual-gamepad channel | 3 | **3** | unchanged |
| Plugin index schema | 1 | **1** | unchanged |
| `api/openapi.json` | 0.28.0 | **0.28.0** | the management API surface did not change; the file keeps the stamp it was regenerated under |
| gamescope patch level (`+pfhdrN`) | 7 | **7** | unchanged — the patch series is untouched |
| `@punktfunk/host` (SDK) | 0.1.4 | **0.1.4** | unchanged |
| `@punktfunk/plugin-kit` | 0.4.1 | **0.4.1** | unchanged |

### ⚠ Breaking changes

- **C ABI 19 → 20, addition only.** `include/punktfunk_core.h` gains exactly one declaration,
  `punktfunk_connection_mgmt_port(const PunktfunkConnection *, uint16_t *)` — the management-API
  port the host advertised in its `Welcome`, or the documented default when it advertised none.
  Nothing is removed or reshaped; an embedder that compares `PUNKTFUNK_ABI_VERSION` at build time
  rebuilds against the new header and is done. Nothing in-tree compares it at runtime.
- **The Windows MSIX package identity changed.** Releases are now signed by Azure Artifact Signing
  (below), and the MSIX manifest `Publisher` must equal the signer subject byte-for-byte — so it
  moved from the self-signed `CN=unom` to the verified subject. Package identity is Name +
  Publisher: Windows treats the new package as a different app, and an in-place upgrade is
  impossible by design. One-time uninstall + reinstall for MSIX installs; the `.exe` installer and
  winget-via-installer paths upgrade normally.
- **Android embedder edge, additive:** `NativeBridge` gains `nativeHostMgmtPort`, and the native
  discovery record gains its 9th field, `mgmt` (the record's append-only rule; 0, non-numeric and
  out-of-range all parse as unknown). Out-of-tree JNI callers are unaffected unless they want the
  value.

### The management port is movable, survives, and is learned in-band

47990 is the management API's port and also the web-UI port of Sunshine and its forks — with the
GameStream planes off, the only port the two still contend for. Moving it now actually works, end
to end:

- **`PUNKTFUNK_MGMT_BIND` joins `host.env`** (the `PUNKTFUNK_GAMESTREAM` shape: env or CLI flag,
  the flag wins), so the choice survives package upgrades that rewrite the unit file. `serve`
  publishes the port it *actually bound* to `~/.config/punktfunk/mgmt-endpoint` (KEY=VALUE, written
  write-then-rename), and the console, the Windows service and the unit files all derive from that
  one file; the six hardcoded 47990 literals survive only as the old-host fallback.
- **`Welcome.mgmt_port`** — a trailing `u16` after the cipher block, the same additive discipline
  as the eight fields before it, so `WIRE_VERSION` stays 2 and an older peer stops earlier and uses
  the default. ⚠ One encode subtlety, pinned by test: `cipher` used to be emitted only when
  non-default, and appending the port to an AES `Welcome` would land its low byte at offset 68 —
  exactly where every shipped 0.28.x client reads `cipher`, fail-closed. `encode` therefore writes
  an explicit cipher byte whenever a port rides along; a host advertising no port still emits
  exactly 68 bytes. The standalone `punktfunk1-host` binary advertises `0` (it has no management
  API).
- **Clients persist it**: `KnownHost.mgmt_port` + `effective_mgmt_port()` across the Rust clients
  (three-rung: live advert → stored → default), the session console, Android (through
  `DiscoveredHost`), and Apple — where `StoredHost.mgmtPort` had existed all along but nothing ever
  wrote it, so every Apple client resolved 47990 regardless. A host that has never been seen over
  mDNS (VPN, routed subnet, multicast-dead network) now learns the port from the authenticated
  connection itself.
- **`PUNKTFUNK_NATIVE_PORT`** completes the pair for the data plane — `--native-port` was CLI-only
  and died on upgrade. A bad value is a startup **error**, not a silent fall back to 9777.
- The Windows shell's half of the client-side learn landed separately (#241): `trust.rs` re-exports
  `learn_mgmt_port`, the shell's own mDNS browser parses the `mgmt` TXT, and `HostTarget` carries
  the port like the mac client's target does.

### Linux thread priority: the renice was a no-op on every install to date

`boost_thread_priority`'s `setpriority()` needs `CAP_SYS_NICE` or a raised `RLIMIT_NICE`; no
channel granted either, and the host binary can never carry a file capability (a capped process's
`/proc/<pid>/exe` is unreadable to KWin — the 0.26.0-1 incident). So capture/encode/send ran at
nice 0, and a shader-compile storm could deschedule them hard enough to stutter audio and drag ABR
to its floor at zero loss. Now:

- **RealtimeKit fallback** — `MakeThreadHighPriorityWithPID`, the same unprivileged broker
  PipeWire clients use. Only the nice verb, never `MakeThreadRealtime`; nothing enters the
  permitted set, KWin identification is untouched.
- **The audio plane is boosted at all, for the first time**: the 5 ms Opus
  capture→encode→send loop, the PipeWire capture mainloop, and the pad-audio streamer (on Windows
  too, via the existing `SetThreadPriority` arm).
- **Packaging ships headroom for rtkit-less boxes**: `packaging/linux/50-punktfunk-nice.conf`
  (`user@.service.d`, `LimitNICE=-15` — a limit, not a grant; effective from next login) on rpm,
  Arch and deb, written to `/etc/systemd/system/user@.service.d` by the Steam Deck installer; deb
  and rpm gain a weak `Recommends: rtkit`, Arch an optdepends hint, and the NixOS module sets
  `security.rtkit.enable = mkDefault true`.

### Host capture gain works on `punktfunk/1`, and boosting no longer hard-clips

`PUNKTFUNK_AUDIO_GAIN` existed only on the GameStream plane, and where it applied it was a hard
`clamp(-1.0, 1.0)` — flat-topping, so pushing past ~1.5× sounded broken long before it got loud
(WASAPI loopback taps upstream of the endpoint's master volume, so the host's own slider never
changes the sent level either). `punktfunk_core::audio::apply_gain` now serves **both planes** with
a tanh soft knee above `SOFT_LIMIT_KNEE` (0.7, ≈−3.1 dBFS): C1-continuous, bounded by
construction, odd-symmetric, memoryless (zero added latency). Unity is a no-op inside the function
itself, so the default wire stays byte-for-byte identical. `capture_gain` rejects non-positive
values and caps at 8.0 (+18 dB). This buys headroom, not loudness — it is deliberately not a
compressor, and the docs say so. `SOFT_LIMIT_KNEE` is excluded from cbindgen on purpose.

### Windows binaries are signed by Azure Artifact Signing

Account `unomsigning`, profile `unom-io`, signed by a service principal holding only the
profile-scoped signer role. Azure mints a **per-request leaf that expires in ~3 days**, which
changes two rules: a timestamped countersignature is now *mandatory* (the old retry-without-
timestamp fallback is a hard failure in Azure mode — it would ship an artifact that goes untrusted
days later, everywhere at once), and leaf pinning is structurally impossible (the updater's
`AUTHENTICODE_SHA256` note claiming otherwise is corrected). `pack-msix.ps1` reads the signature
back off the packed `.msix` and fails on Publisher drift. Driver catalogs are deliberately
untouched: they keep the `DRIVER_CERT_*` cert and the installer still plants it as a machine root
(PnP trust is independent of SmartScreen/UAC trust). Canary and fork builds keep the `.pfx` and
ephemeral fallbacks.

### Library: a launcher the host cannot open no longer costs the whole sync

`valid_launcher_ui` conflated vocabulary with environment. It is now split: `known_launcher_ui`
(an unknown launcher kind is a plugin bug — still a hard 400) and `resolvable_launcher_ui` (the
launcher just is not installed on this box — the entry is dropped with one warn and the games
sync). Same shape as the unservable-cover fix, on the launch side. And Playnite is actually
findable now: the old lookup read the LocalSystem service's own HKCU and `%LOCALAPPDATA%` (the
SYSTEM profile — a per-user Playnite is invisible there) and matched a registry key name Inno Setup
never writes. Now: every loaded hive under `HKEY_USERS` plus both HKLM views, matched on
`DisplayName`, then `C:\Users\*\AppData\Local\Playnite`.

### Hyprland/sway capture: six defects, all ours, and streaming now survives past one session

The wlr portal route looked environmental and never was. Measured on Hyprland 0.55.4 +
xdg-desktop-portal-hyprland 1.3.12, fixed in one arc (#240):

- **The dmabuf pod offered `BGRx`; xdph offers `BGRA`.** The modifier lists intersect perfectly,
  the fourcc never does, so PipeWire failed the link itself (`no more input formats`) — and the
  pods live only in the PipeWire *daemon's* log, which is why it read as a GPU/modifier problem.
- **A per-cast tokio runtime orphaned ashpd's process-global D-Bus connection.** ashpd caches its
  connection in a `OnceLock`; the first cast's runtime hosted zbus's reader task and then died
  with the cast, so the first stream of a host process worked and every later one went black.
  Both wlr backends now share one long-lived portal runtime.
- **Teardown removed the captured output before closing the cast**, and xdph spun on the wreckage;
  the order is now cast-then-output.
- **A hung portal handshake leaked its thread** and the leak poisoned every later cast; the
  handshake is now bounded.
- **The wlr absolute-motion injector aimed at the operator's head**, never the streamed one; the
  pointer is now bound to the streamed output.
- **The cursor park schedule read a missing cursor overlay as a lost pointer** — an Embedded-mode
  portal never sends one.

### Everything else an integrator might notice

- **vdisplay/KDE:** a bare-spawn gamescope session under an exclusive topology now darkens the
  physical panels over `org_kde_kwin_dpms` (new in-process `kwin_dpms` module,
  `kscreen-doctor --dpms` fallback), refcounted host-wide so concurrent spawns compose; DPMS is
  non-persistent, so a dead host leaves nothing to journal. Managed and Attach routes untouched.
- **macOS client:** `Settings::inhibit_shortcuts` is finally implemented on Apple — a local
  keyDown monitor claims every ⌘ chord while input is captured and forwards it host-side (AppKit
  dispatches menu key equivalents before the stream view sees them, so ⌘Q used to quit the
  client). ⌘⎋ and ⌃⌘F stay client-side; ⌘Tab/⌘Space/Mission Control are out of reach without a
  CGEventTap. Chord matching no longer compares Caps Lock and `.function`/`.numericPad` bits raw.
- **Android client:** `Gamepad.padButtonBit` resolves a gamepad-sourced `KEYCODE_BACK` to
  `BTN_BACK` — pads that report Select as plain BACK (the Android-TV shape) no longer quit the
  stream on one press, and the Select chords (exit chord, mic mute, stats tier) become reachable
  on exactly those pads. `FLAG_FALLBACK` events stay excluded.
- **CI:** Android canaries now feed Play **open testing (beta) and closed testing (alpha)** from
  one Play edit (`play-upload.py --also-track`); tags still publish production only, and a manual
  `android.yml` dispatch can now opt into publishing (`publish=true`), so a lost merge run is no
  longer a dead end. Windows
  runners provision the .NET 8 runtime and a machine-wide signing client (a mixed-mode dlib with
  no runtime makes signtool exit 3 in silence).



60 commits since v0.28.0.

A patch release in the strict sense: **nothing on the wire, in the C ABI, in the driver protocol or
in the plugin contract moves.** Every host, client, driver and plugin built against v0.28.0 keeps
working against v0.28.1 and vice versa, in both directions and with no re-pairing.

### Versions

| | v0.28.0 | v0.28.1 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged |
| C ABI | 19 | **19** | unchanged — `include/punktfunk_core.h` is byte-identical to the v0.28.0 tag |
| Rust edition | 2024 | **2024** | unchanged |
| MSRV (`rust-version`) | 1.85 | **1.85** | unchanged |
| Workspace crate dirs | 27 | **27** | unchanged |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3) |
| Windows virtual-gamepad channel | 3 | **3** | unchanged |
| Plugin index schema | 1 | **1** | unchanged |
| `api/openapi.json` | 0.27.0 | **0.28.0** | the management API **did** change (two collection deletes, below); the file carries the stamp it was regenerated under, not `0.28.1` |
| gamescope patch level (`+pfhdrN`) | 6 | **7** | 8 patches → 9 (the linger crash); no new capability |
| `@punktfunk/host` (SDK) | 0.1.4 | **0.1.4** | unchanged |
| `@punktfunk/plugin-kit` | 0.4.1 | **0.4.1** | unchanged |

⚠ **The `api/openapi.json` stamp is not a per-release counter** and should not be read as one. The
drift test (`openapi_document_is_complete_and_checked_in`) normalizes `info.version` on both sides,
so only the *surface* is gated and a version bump alone never invalidates the snapshot. The table
row says what the file actually says. Regenerating it needs a Linux or Windows host build —
`punktfunk-host` does not compile on macOS.

### ⚠ Breaking changes

**None.** No wire change, no C ABI change, no driver-protocol change, no plugin-contract change.
Three things are worth an embedder's or packager's attention anyway, none of which break a build:

- **The Rust crate gained one public constant.** `punktfunk_core::client::FLUSH_COOLDOWN` was
  `pub(crate)`; the host now compares against it rather than against a copy of the number (see the
  keyframe-cadence fix below). Addition only.
- **`NativeBridge.nativeStartAudio` takes a third argument** on Android — `isTv`. Detail in the
  Android section; this is a JNI signature change, so an out-of-tree caller must pass it.
- **Every Linux packaging channel now ships a second gamescope artifact**, the Vulkan WSI layer,
  and a package that carries the compositor without it is *fatal* rather than degraded. If you
  repackage `punktfunk-gamescope` downstream, read the gamescope section before rebuilding.

### The management API gains two collection deletes — "unpair all"

Clearing a host's trust store meant one row-level delete per device, each with its own
confirmation. Two new endpoints, one per pairing plane:

```
DELETE /api/v1/clients          -> {"unpaired": N}
DELETE /api/v1/native/clients   -> {"unpaired": N}
```

They are **not** a loop over the per-fingerprint deletes. Each empties its store in ONE persisted
write, because N deletes would rewrite and atomically rename the store N times and a failure
partway leaves a half-emptied store with nothing saying which half. The two planes are separate
endpoints because they own separate trust stores with separate persistence and separate revocation
duties.

Being collection deletes, they carry the single delete's revocation guarantees across the whole
set: a live session owned by any removed certificate is ended, and on the GameStream side the ENet
control port (UDP 47999) closes, because no pairing is left to hold it open.

**200 with a count, not the single delete's 204/404.** "Unpair everything" is idempotent — an
already-empty store satisfies it — and the count still distinguishes three devices from none.

⚠ **Both are admin-token only.** The route-classification gates match on (method, path), so the
roster's plugin-readable `GET` does not carry over to emptying it; both new routes have explicit
rows in the table, like every other pairing-administration route. The native endpoint answers
**503** on a host built without that plane, which is why the console calls only the planes that
actually have a row.

`UnpairAllResult` is the one new schema. `api/openapi.json` is regenerated;
`docs-site/public/openapi.json` is re-synced from it (see **Documentation** at the end).

### The pad-audio "Wireless Controller" speaker hides while no client pad is attached

Field-confirmed (2026-08-14, the same Helldivers 2 reports as below): the per-pad audio endpoint
the Windows host mints — a Steam-Streaming-Speakers instance stamped with a DualSense's name,
container and 4 ch/48 kHz formats, **pre-provisioned at every host start** — is deliberately
indistinguishable from a real DualSense speaker. That disguise is the feature during a pad
session (libScePad titles route haptics audio at it) and a trap the rest of the time: an idle
Helldivers 2 finds the endpoint by identity, engages its DualSense-haptics path against a device
nothing services, and drops to 2–5 FPS 1% lows — with the host completely idle, no controller
plugged in, and no session ever run. The reporter isolating "the DualSense speaker" and disabling
it in mmsys.cpl restored full performance; that manual remedy is now automatic.

The endpoint now parks **hidden** (`DEVICE_STATE_DISABLED`, via `IPolicyConfig::
SetEndpointVisibility` — the exact call behind mmsys.cpl's Disable) whenever no client pad is
attached: provisioning hides it at startup (and a `PUNKTFUNK_PAD_AUDIO=0` host hides leftovers
from earlier runs), the per-pad streamer shows it for exactly the pad's lifetime — to a game,
indistinguishable from a DualSense arriving and leaving. The devnode, driver binding and stamps
stay put, so the flips raise no PnP traffic and the expensive provisioning still happens once at
boot.

⚠ **Operator-visible:** "Speakers (Wireless Controller)" now shows as *disabled* in the Sound
control panel while no client pad is connected — that is the parked state, not a defect. The
`pad-endpoint` devtest grew `show`/`hide` verbs; `tone`/`capture` need a `show` first.

### An idle Windows host no longer owns the box's default microphone

Field report (the second Helldivers 2 one — the first led to v0.28.0's mint-retry fix): with the
host **idle**, a locally played Helldivers 2 tanks to 2–5 FPS 1% lows, and Windows' own Sound
settings Recording tab goes unresponsive. Root cause: the audio wiring pass asserted *default
recording = the virtual mic's capture side* on **every** pass, including the mic pump's eager
boot pass — and `SetDefaultEndpoint` covers eCommunications, so every game's voice input bound a
virtual microphone whose feeder only runs during a stream. Nothing ever restored it: not session
end, not service stop. Games that hold an always-open voice capture (Helldivers 2 is Wwise +
in-game voice — its own wiki calls the game "finicky with audio devices") stall on that dead
endpoint.

The recording default is now **session-scoped**, exactly like the playback default has always
been: parked on the virtual mic only while a desktop-audio capture is open, the operator's device
remembered (plus an on-disk crash marker, `audio-default-rec.prev`), restored when the capture
closes, recovered at next boot after a crash, and unparked by the uninstaller. A game launched
*during* a stream still records the client's mic; one launched before the stream keeps the
operator's own microphone.

Boxes wedged by earlier builds (which recorded nothing to restore) heal themselves: an idle
wiring pass that finds the default recording sitting on the plan's mic capture moves it back to
the first real microphone.

⚠ **Operator-visible:** outside a stream, the default recording device is now whatever you set —
Punktfunk only takes it for the duration of a stream. If you *want* apps to record the client mic
while idle, select "Punktfunk Microphone" manually; the host no longer re-asserts it (idle
re-assertion used to stomp a manual choice within one mic-pump reopen).

### The NixOS module started a second host in root's systemd, which stole the ports from the real one

Found on the first real deployment of `packaging/nix/nixos-module.nix` (NixOS 26.05, punktfunk
0.28.0-nix). The host crash-looped forever on one line:

```
ERROR punktfunk_host: start RTSP server: bind RTSP 48010: Address already in use (os error 98)
```

`systemd.user.*` has no per-user form in NixOS: it installs units into **every** user's systemd
manager. `host.autoStart` then adds them to `default.target` — for every user, including **root**,
whose `user@0.service` springs into existence the moment anybody so much as SSHes in as root. Root's
copy of the host won the race for the fixed ports, and the desktop user's copy could never bind.

The failure is nastier than it sounds because every *other* listener binds first and logs success —
the version banner, mDNS on 47989, the GameStream warning all print normally — so the log reads like
a conflict with some unrelated program. A second copy of *itself*, running as root, is the last
thing anyone looks for. `host.users` did not help: that option only granted `input`/`punktfunk`
group membership and never scoped the units.

Fixed by rendering `ConditionUser=` on all four user units (`punktfunk-host`, `punktfunk-web`,
`punktfunk-web-init`, `punktfunk-scripting`) from `host.users`. Each entry is written `|user` — the
pipe makes it a *triggering* condition, which systemd ORs; plain repeated `ConditionUser=` lines are
ANDed and would have matched nobody. With `host.users` empty the units fall back to
`ConditionUser=!@system`, which still keeps root out while leaving a normal login free to run the
host by hand, as the module header documents.

`packaging/nix/module-check.nix` gained three assertions covering both branches and the fact that
`punktfunk-web-init` keeps its pre-existing (non-triggering) `ConditionPathExists` alongside the new
condition. They run in the `eval` leg of `nix.yml`, and were verified to fail against the unfixed
module before being committed.

### The Steam plugin synced nothing on Windows: its art is in Program Files, the art roots were not

Field report — the plugin installed, the grid stayed empty, and the only clue was one host warn per
sync:

```
plugin:steam sync (fs-change) failed: HostRequestError: PUT /library/provider/steam?store=steam
  failed: art.hero: local art must be an image file (…) inside an allowed art root
```

Two independent defects, both fixed here.

**1. Steam's art was never inside an allowed root on Windows.** `art_roots()` defaulted to the users
base (`C:\Users`, from `%PUBLIC%`'s parent), which covers the launchers that install per-user —
Playnite under `%APPDATA%`, Heroic under `%APPDATA%` — but *not* Steam, which installs to
`C:\Program Files (x86)\Steam` and keeps both the art the plugin publishes there:
`appcache\librarycache\<appid>\<hash>\` and each account's `userdata\<id>\config\grid\` overrides.
Every cover the plugin emitted was out of root. This is a v0.28.0 regression: the built-in scanner
the plugin replaced served its covers through the legacy `steam:` art-proxy branch, which never
passed through the H-2 confinement — deleting the scanner routed that art through a gate it had
never been measured against. `art_roots()` now also includes every Steam install root it can find,
from `%ProgramFiles(x86)%` / `%ProgramFiles%` / `%ProgramW6432%` and from HKLM
`Valve\Steam\InstallPath` (so a Steam on another drive is covered too). POSIX needed no equivalent —
every Steam layout there, native and Flatpak, is already under `$HOME`.

This does not weaken the confinement. It exists to stop the host (SYSTEM) reading files the plugin
lane (LocalService) cannot reach itself; the Steam directory is readable by LocalService already, so
nothing there is reachable *because* the host is privileged. The extension, regular-file, magic-byte
and config-dir gates all still apply, so Steam's own `config.vdf` and `ssfn*` credential blobs are
not servable from it — there is a test.

**2. One unservable cover threw away the entire library.** `PUT /library/provider/{p}` validated art
per entry and returned 400 for the whole payload on the first bad value, so a path mismatch cost the
operator *every game from that store*, not a thumbnail — and the plugin, which only ever sees
`HostRequestError`, could not say which. A provider reconcile now **strips** unservable local art and
syncs the rest (`sanitize_art_paths`), logging one aggregated warn naming the count, an example path
and the env var. The invariant the 400 held is unchanged: no unservable path is ever persisted. The
operator's own single-entry custom writes keep the hard 400 — there the path was typed by hand, and
silence would be the wrong answer.

⚠ **Operator-visible:** an art-root mismatch no longer fails a sync. If covers are blank where you
expect art, the cue is the host log's `dropped local art the proxy may not serve` line, and the knob
is `PUNKTFUNK_LIBRARY_ART_ROOTS` (which **replaces** the defaults — list every root you need).

### Hyprland/Sway — the wlr-family backends asserted a cursor mode instead of negotiating it

🛑 **Every cursor-forward session on current Hyprland died at `select_sources`** — "pipeline build
failed" and a black client, with `unavailable cursor mode 4` in the portal log.

Hyprland and wlroots both hardcoded portal `CursorMode::Metadata` whenever the session had
negotiated the cursor channel, and never asked the backend what it supports. That is **not** a soft
failure: xdg-desktop-portal's **frontend** validates the requested mode against the backend's
`AvailableCursorModes` and fails the call with `"Unavailable cursor mode %x"` before the backend
ever sees it.

⭐ **Measured on glass 2026-08-14, and worse than the report suggested.** Against a live Hyprland
0.56.2 with xdg-desktop-portal-hyprland 1.4.1 and xdg-desktop-portal 1.22.1 — all current —
`AvailableCursorModes` reads **3** (`Hidden|Embedded`) on both the backend impl interface and the
frontend. **xdph does not offer the metadata cursor at all**, so this broke every cursor-forward
session on current Hyprland, not merely on old installs, and **updating the portal would not have
helped.** xdpw is the same from the other end: its `screencast.c` refuses `METADATA` outright.

`pf-capture`'s own portal path has always negotiated (`choose_cursor_mode`); this restates that
ladder in `pf-vdisplay`, which may not depend on `pf-capture`. The downgrade is graceful rather than
merely survivable: with the portal on `Embedded` no `SPA_META_Cursor` arrives, so the host feeds the
cursor channel nothing and a cursor-forward client draws nothing of its own — **one pointer, not
two.**

**`PUNKTFUNK_PORTAL_CURSOR_MODE=auto|hidden|embedded|metadata`** pins the preference for a backend
that advertises a mode it implements badly, which negotiation cannot detect. It is a preference
only: a pin runs the same ladder, so no value can re-create the refused request.

⚠ The module is declared **unconditionally**, so its ladder tests run on every CI leg rather than
only the one that compiles `mod hyprland` — including a Linux-only test pinning our bit values
against ashpd's enum (ashpd answers 4 for `Metadata`, the number in the report), verified
non-vacuous by planting a wrong discriminant.

### Android — the audio plane trusted AAudio, and a TV box that opened a stream it never played was silent for the session

🛑 **Reported from the field: no audio at all on an NVIDIA Shield Android TV, stereo, with the same
host and settings that play fine on an Apple TV.** Video unaffected. Turning off the client's
low-latency mode — which is what gates the forced HDMI mode switch and the `usage=Game` tagging —
changed nothing.

The Android client opens AAudio directly (the Apple client goes through AVAudioEngine, which
reconfigures itself on a route change; that difference is why this was Android-only). Opening
AAudio is a negotiation with a vendor HAL, and this plane treated it as a formality: one Exclusive
attempt, one Shared retry, and everything after the open taken on trust. **Three distinct failures
all presented as "the app has no sound" behind a healthy-looking log**, and none of them was
detected:

- **A configuration that opens but routes nowhere.** Nothing ever checked that the device actually
  pulled a sample, so the decode thread would happily decode Opus into a dead stream forever.
- **`request_start` failing.** The old code gave up on the spot instead of trying anything else, so
  one unhappy configuration disabled audio for the whole session.
- **A disconnect.** By AAudio's contract a disconnected stream is dead and the only recovery is
  close + open a new one. The error callback logged a warning and did nothing else — so an HDMI
  mode switch, an AVR re-handshake or any route change meant silence for the rest of the session.
  On a TV that is not a rare event: the client itself drives an HDMI mode switch on the video
  plane, and the platform's own match-content-frame-rate setting drives more.

The open now walks a **ladder**, every rung has to **prove the device is pulling** before it is
accepted, and a **supervisor** owns the plane for the session and reopens it when the device goes
away (bounded retries across the settling time of a route change, so a reopen landing mid-switch
does not permanently disable audio). The granted rate/channel-count/format are checked against what
was asked for rather than assumed — the realtime callback casts AAudio's buffer to `f32` and writes
`num_frames × channels` of them, so a HAL that disagreed was an out-of-bounds write on the audio
thread, not merely a mistuning.

⚠ **Behaviour change on TV boxes: they now start at Shared instead of Exclusive.** Exclusive is
MMAP, the lowest-latency path AAudio has, and the one rung whose routing cannot be verified from
inside the process. The latency it buys here was never actually banked — the jitter-ring depths are
unchanged from the Shared-only era (`JitterTuning::AAUDIO` still primes at 25 ms) — so on a
mains-powered HDMI box the few ms are worth less than not betting the audio plane on it. Phones,
tablets and handhelds are unchanged and still try Exclusive first. If no rung proves itself, the
first one that opened and started is used anyway: the watchdog must never be able to turn working
audio into no audio.

⚠ **Embedder-visible:** `NativeBridge.nativeStartAudio` takes a third argument, `isTv`
(`FEATURE_LEANBACK`, the same source the video plane already used).

Three new sysprops bisect all of it on a device that cannot be handed a custom build, alongside the
existing `debug.punktfunk.no_av_sync`: `debug.punktfunk.audio_sharing` (`exclusive`|`shared`),
`debug.punktfunk.audio_perf` (`lowlatency`|`none`) and `debug.punktfunk.audio_reopen` (`0` pins the
old give-up-on-disconnect behaviour). A stream that stops taking samples after it started now says
so at `error` level instead of looking exactly like an app with no sound.

### gamescope — we ship our own Vulkan WSI layer, so a game can reach an HDR10 swapchain (⚠ packager-visible)

🛑 **On essentially every box running a distro gamescope, no game could render HDR at all** — and
nothing said so.

A game nested under gamescope gets an HDR10 swapchain from the FROG WSI layer and from nothing
else: gamescope advertises no runtime colour-management protocol a Mesa/NVIDIA WSI could negotiate
through. That layer speaks `gamescope_swapchain` to the compositor, and when the two disagree the
compositor rejects the client's `swapchain_feedback` and **every Vulkan client dies on a black
screen** with sound and input intact and no error anywhere.

We shipped our own compositor and *not* a layer, on the recorded grounds that the layer is
"version-independent of the compositor binary". It is not — `wsi_layer_matches_our_gamescope()`
exists precisely because it is not — so the host was left guessing from version triples, and that
guess is wrong in both directions. A distro at the same upstream tag that patched the protocol
compares EQUAL and keeps a layer that will black-screen every game; a distro at a different tag
with a byte-identical protocol compares unequal and loses HDR for nothing. **Since we pin a rev,
the second case is the normal one.**

We now build the layer from the same tree at the same rev as the compositor and ship it, so the two
cannot drift and the guess stops being load-bearing. It installs under **our own** name
(`VK_LAYER_PUNKTFUNK_gamescope_wsi`), at our own path, with our own enable/disable variables, so it
coexists with the distro's rather than colliding — the Vulkan loader keys implicit layers on that
name — and the host switches the two independently within one session.

`WsiPlan` resolves three states once per launch (the fallback spawns `--version` probes):

| state | condition | action |
|---|---|---|
| `Ours` | our layer is installed | enable ours, force the distro's off — **both halves, or it is a bug** |
| `DistroKept` | no layer of ours, distro's looks compatible | touch nothing |
| `DistroDisabled` | no layer of ours, distro's untrusted | v0.28.0's behaviour |

That last arm is the fail-safe: a host newer than its gamescope package behaves exactly as it did,
rather than enabling a layer that is not there.

⚠ **What packagers must know.** The layer manifest carries an **absolute** `library_path` baked in
at build time, so every channel installs the `.so` at exactly that path: literal
`/usr/lib/punktfunk` — **not** `%{_libdir}` (which is `/usr/lib64` on Fedora) and not a Debian
multiarch triplet. Nothing links it by soname (the loader `dlopen`s it by that path), so multilib
has no claim. rpm and nix read the path back **out of the manifest** and fail if it names a file the
package does not install, because a manifest pointing at nothing is the silent shape of this bug.
A missing layer is **fatal in every channel**, not best-effort: a package carrying the compositor
without it looks completely healthy and then silently denies every game an HDR10 swapchain.

The packaging scripts now take `--stage` (the DESTDIR the gamescope build script wrote) instead of
a path to one binary, and CI caches the whole staged tree; the `gs-cache` key already hashes
`packaging/gamescope/**`, so stale caches in the old single-file shape cannot be restored into the
new layout. The manifest rewrite lives in `packaging/gamescope/rewrite-wsi-layer-manifest.py`
rather than a heredoc, because the FHS builds and the Nix store both need it and must rename the
layer identically. **NixOS has no `/usr`**, so the layer lives inside the gamescope derivation and
the host's path is overridable with **`PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR`**, which the module sets
— the same posture as `PUNKTFUNK_GAMESCOPE_BIN`.

### gamescope — HDR sessions anchored SDR white a stop bright, and never said game HDR was unreachable

🛑 **Field report: Steam's Big Picture UI glaring and over-saturated while HDR game content looked
washed out, on the same stream.** Those are one error.

gamescope maps everything that is not an HDR game — the desktop, the Steam overlay, an SDR title —
into the session's PQ container at `--hdr-sdr-content-nits`, and we passed that flag **only** when
an operator had set `PUNKTFUNK_GAMESCOPE_SDR_NITS`. Unset, gamescope used its own default of
**400**, while every first-party client anchors diffuse white at **203** (BT.2408 reference white;
the Apple presenter hands exactly that to `CAEDRMetadata.hdr10`'s `opticalOutputScale`). The two
ends sat nearly a stop apart, so the UI landed above SDR white and the client's tone-mapper worked
from a reference point the host had never used, flattening the content around it.

**The flag is now always passed, defaulting to 203.** `PUNKTFUNK_GAMESCOPE_SDR_NITS` still
overrides it for anyone who wants a brighter or dimmer desktop — it is the anchor, not a taste
knob. ⭐ Because it is an env var, a field A/B needs **no rebuild**.

Separately, and visible in the same log: the two HDR decisions in a gamescope session were made
independently. `hdr_args()` never consulted `wsi_layer_matches_our_gamescope()`, so when the layer
check fired the session launched **advertising HDR while having made an HDR10 swapchain
unreachable for every game in it** — a title told to render HDR rendered it into an SDR swapchain
and looked washed out, with nothing anywhere saying why. It now warns. The behaviour of the check
itself is deliberately unchanged; the section above is the real fix.

### punktfunk-gamescope `+pfhdr7` — a lingered session no longer dies of its own capture teardown

🛑 **On client disconnect the host keeps the headless gamescope alive so a reconnect resumes the
same session — and gamescope could SIGSEGV in exactly that window, so the kept display was dead and
reconnect silently got a fresh compositor with the game lost.** When the capture consumer leaves,
PipeWire's `remove_buffer` (and the stale-push path in `dispatch_nudge`) destroyed idle buffers on
the **PipeWire thread**; dropping the last `CVulkanTexture` reference there calls into the Vulkan
driver (`vkDestroyImage`/`FreeMemory`/dmabuf fds) while steamcompmgr can still be inside
`vulkan_screenshot` on another buffer of the same 4-buffer pool. On NVIDIA that races to a SIGSEGV
in `CVulkanCmdBuffer::insertBarrier` — timed at stream end, which is why it selectively killed
linger. The journal signature: linger line → coredump → `kept display was dead — recreating`.

Patch 0009 queues those corpses on the PipeWire thread and has steamcompmgr reap them on every
vblank — including while the stream is paused, which is precisely the linger state. Found, fixed
and proven live by **luxus** ([punktfunk-overlay#9](https://github.com/luxus/punktfunk-overlay/issues/9)):
four coredumps on 4K60 HDR + composited cursor, zero after; disconnect/reconnect now reuses the
lingered session. Banner `+pfhdr6` → `+pfhdr7` (no new capability — but "reconnect lost my game"
triage must be able to read a box's exposure off its banner, the same rule as `+pfhdr5`/`6`).

### Apple — the stats overlay lied three ways, and every host-anchored number with it

🛑 **Two sessions minutes apart on the same wire read `hostnet_p50` 17–21 ms, then a physically
impossible 4.4 ms** — host-side encode alone is ~4.7. Three independent defects, all of which
corrupt any measurement taken against a host clock:

- **A frozen clock-offset.** The client consumed the **connect-time** skew offset and cached it —
  in a `Stage2Pipeline` field, in a `StreamPump` `let`, and in a `ContentView` closure **capture
  list** feeding the hostnet meter and the host/network splitter. The core keeps a *live* estimate
  (`punktfunk_connection_clock_offset_now_ns`, ABI v10, re-synced every 60 s and on suspected
  wall-clock steps) whose own doc says the connect-time value "silently corrupts every
  capture-clock comparison" after an NTP step — **and a VM host steps.**
  `PunktfunkConnection.clockOffsetNs` is now the live read (an atomic load behind the FFI), read at
  use: per record, per AU, per enqueue. The Swift audio plane's AvSync observation takes the same
  live value.
- **Silently trimmed impossible samples.** `LatencyMeter`'s guard (≤ 0 after offset correction)
  dropped samples without counting them, so a wrong offset did not invalidate a window — it trimmed
  the impossible half of the shifted distribution and presented the surviving tail as a plausible
  small number. That is the origin of the historical "0 ms network / 0 ms e2e" readings. Refusals
  are now counted and drained **separately from `Stats`** — deliberately, because a fully-poisoned
  window drains to `nil` and a count inside `Stats` would vanish with it. The HUD shows an orange
  **`clock offset suspect`** line and the stats line grew **`skew_trim=N`**; nonzero means
  disregard `e2e`/`hostnet` for that window.
- **`-1` fallbacks printing as `NaN`.** In a `CVarArg` context `cond ? someDouble : -1` does **not**
  unify to `Double` — the literal goes in as `Int`, and `%f` reads `Int64(-1)`'s all-ones bit
  pattern, which is a quiet NaN. Latent since the 1 Hz stats line existed. All fallbacks are now
  typed `-1.0`.

⚠ **Any client-side e2e or hostnet figure recorded before this release is suspect** and worth
re-measuring rather than trusted as a baseline.

Two new levers ship with the tvOS present-floor investigation, both env-only:
**`PUNKTFUNK_FRAME_LATENCY`** (float 0…4, default 1) makes the `preferredFrameLatency` ask
adjustable, so an on-device ladder can establish whether the property does anything on tvOS — the
previous "immovable two-refresh floor" verdict rested on a **readback** of a plain read-write
float, which is not a grant. **`PUNKTFUNK_PRESENTER=stage1` now resolves on Release builds** (the
persisted picker stays DEBUG-gated; an env var takes a `devicectl`/Xcode launch to exist, so it is
never a leftover). Stage-1 presents on the hardware video plane rather than through the GPU
compositor — the one rung that can dodge the two-refresh regime — and the field A/B that concluded
otherwise had silently run stage-4, because the gate keyed on build config.

### Apple — two colour faults: an SDR stream shipped untagged, and it forced the TV into HDR10

- **The SDR layer was never tagged.** `configure(hdr:)` guards on `hdr != hdrActive` and
  `hdrActive` starts `false`, so a session that is SDR from its first frame matched the initial
  state, fell through the guard, and `configureColor` never ran once — the layer kept `make()`'s
  bare configuration, which assigns no colour space. An untagged `CAMetalLayer` gets no colour
  matching: a BT.709 stream is drawn in the display's native space. Mild oversaturation on a P3 Mac
  or iPad; on a tvOS display composited for HDR it also lifts the black floor. ⚠ It also made
  `PUNKTFUNK_SDR_COLORSPACE` **dead code on exactly the sessions it exists to fix**, so a field A/B
  of that knob would have shown no change.
- **An SDR stream drove an HDR-capable TV into PQ output.** `applyDisplayCriteriaIfNeeded` builds a
  synthetic format description hardcoding BT.2020 primaries, ST.2084 and the BT.2020 matrix, then
  hands it to `AVDisplayManager` — and its guard checked only that no criteria had been set and that
  the user's HDR *setting* was on, never that **the stream** was HDR. That setting defaults to true.
  The Apple TV switches HDMI to limited range in its HDR modes, so a set configured for full range
  renders code 16 as grey rather than black. Now gated on `connection.isHDR` as well; layout re-runs
  it, so a session that flips to HDR mid-stream still picks the mode up.

### Apple — the macOS device-change recovery could answer itself forever (mic on)

**Streaming from a Mac with the microphone enabled cut audio AND input on a ~2.5 s metronome
while video ran untouched** (field, 2026-08-14: a Mac Studio whose default input is a 6-channel
device). The chain: the voice-processing engine cannot start on that mic, every rebuild re-tried
it, and the failed attempt's HAL churn (VPIO builds and tears down an aggregate device) stopped
the healthy fallback engines — which posted the `AVAudioEngineConfigurationChange` that scheduled
the next rebuild. Each ~1.9 s rebuild runs on the main thread, where macOS input capture and
sending live, so input froze on the same beat — and since audio, input and mic share the QUIC
datagram plane while video rides its own socket, the wire signature read as a network fault and
the host's METRONOMIC heuristic pointed at the display stack. Three defenses, layered because no
single one covers every feedback shape:

- **A voice-processing start failure latches per input device** (`CombinedTopologyGate`): a
  rebuild goes straight to the split topology instead of re-running a failure that is a property
  of the device. A different default input earns exactly one fresh attempt.
- **A configuration change posted by an engine that is RUNNING is the rebuild's own echo, and is
  ignored**: an engine stops itself before posting, so a live poster was already restarted.
- **Rebuilds that chain anyway back off exponentially** (`RebuildBackoff`: 0.5 s floor doubling
  to a 30 s cap, reset by 10 s of quiet) — an unforeseen loop costs one blip per half-minute
  instead of a metronome, and the chaining itself logs a WARN that names the condition.

iOS/tvOS behaviour is untouched (routes are session-managed there; nothing is latched). Until a
client carries this, the field workaround is turning the client microphone off.

**And the engines no longer start on the main thread at all.** An engine start can block on the
audio server for seconds (~1.9 s per attempt in the field case) and macOS captures and sends the
stream's input from the main thread — so even a single legitimate device switch froze input for
the length of the rebuild, loop or no loop. All engine build/start/teardown now runs on a
per-session serial `engineQueue`; the main queue keeps only the trigger bookkeeping (debounce,
backoff, retry ladder), which is cheap by construction. ⚠ Embedder-visible edge:
`SessionAudio.start()` is now asynchronous on macOS too (it always was on iOS/tvOS) — playback is
live shortly after the call, not on return, and `stats` is safe from any thread.

### Apple gamepad UI — a host menu, and About becomes a page

**UP on a saved tile opens Wake / Copy link / Edit… / Forget pairing / Remove.** The desktop and
Android consoles have had this for a while; this is the Apple port, so the three consoles are
learned once. Wiring UP takes the whole vertical axis away from scrolling (down goes inert) — a
horizontal carousel has no vertical travel to spend, and one meaning per direction is what makes
the gesture learnable. **Remove arms on the first press and fires on the second**, disarming if
focus wanders off the row: the touch grid gets a system confirmation dialog, and a thumbstick from
across a room deserves at least as much. Edit reuses `GamepadAddHostView` seeded from the record and
writes a **copy** back through `HostStore.update`, so the fingerprint, MACs, pins and binding the
form never shows survive a rename; it **replaces** the menu rather than stacking on it, keeping the
shell's "depth ≤ 1 by construction" true. A pinned profile card offers only Unpin — it is a
shortcut, not a second host.

**The start-of-stream shortcut banner is retired.** Telling someone the controls for six seconds,
over the stream they just connected to, answers the question at the one moment nobody is asking it
— and it put a composited overlay above the stream to do it. The words are now a catalogue rendered
in an About page you can open, which is also its own section rather than the last row of Interface.
Its remaining fixes: the identity card became a version line under the rows, a zero-radius clip is
still a clip (it cropped the TV's wide icon), and the card ignored the row column.

⚠ **Apple console screens read the ink they publish.** A SwiftUI screen cannot read the environment
value it publishes in the same view — so a pale palette stayed white-on-white on Apple TV. Fixed
across every console screen.

### Console UI — Skia sized its function table to the loader, not to what we promised

🛑 **On a Steam Deck the console home died on update**, and in a stream the same failure quietly
cost the stats OSD and capture HUD.

The skia-safe 0.87 → 0.99 move swapped `BackendContext::new` for `new_builder(…, None)` and
recorded the `None` as "byte-for-byte what the removed constructor did". True of the **value**,
false of the **behaviour**: `None` leaves Skia's `fMaxAPIVersion` at its `0` sentinel, and the newer
Skia acts on that sentinel by falling back to **`vkEnumerateInstanceVersion()` — the loader's
ceiling, not ours.** The presenter declares 1.3; a current Mesa answers 1.4 (1.4.321 on SteamOS
3.7, host and inside the flatpak sandbox alike). Skia then validates a 1.4 function table against an
instance that only promised 1.3, `vkGetDeviceProcAddr` returns null for the entry points in
between, and `make_vulkan` hands back `None`. At 0.87 the sentinel was inert because that Skia knew
nothing of Vulkan 1.4 — **which is why this surfaced the moment v0.28.0 landed.**

`run.rs` makes an overlay that cannot init fatal for `--browse`, so the Decky panel's button and the
gamepad-UI library shortcut both failed to open. The presenter now publishes
`SharedDevice::api_version` — `min(what we declared, what the loader reports)` — and
`SkiaOverlay::init` passes it instead of `None`. ⚠ `pf-presenter`'s `vk` module is
`cfg(any(linux, windows))`, so this was never Deck-specific.

### pf-vkdecode — AV1's "maximum parameters" level is not a level above the ceiling

🛑 **Every AV1 session demoted to D3D11VA** with `stream level (seq_level_idx 31) above the device's
maxLevel (AV1 Std level 23)` — on hardware decoding the stream trivially on the rung it fell
through to.

`seq_level_idx` is a 5-bit field: Annex A defines 0…23 (levels 2.0…7.3), reserves 24…30, and makes
**31 the "maximum parameters" level — the spec's own way of saying the bitstream is not constrained
to a level.** `StdVideoAV1Level` stops at 7.3 = 23, so 31 has no Std code point and the index-coded
comparison that holds across 0…23 says nothing: `31 > 23` is true even of a device that decodes
everything AV1 can name, which is what makes it useless as a capability test. We write no AV1 level
on any host encode path, so whichever sentinel the vendor's encoder defaults to is what the client
must accept. This is the AV1 half of the same defect fixed for H.264/H.265 in v0.28.0, which was
left alone on the premise that no over-declaration had been seen in the field — the reporter's log
from that same day already showed otherwise.

### Client stats — the stage line is a partition again

A field reader added up `host 5.4 · net 0.3 · decode 6.6 · display 1.4` against `e2e 8.1` and asked
why the parts did not sum. Fair question: they sum **without** `decode`.

The stages *are* a per-frame partition of e2e — pts →(host+net)→ received →(decode)→ decoded
→(display)→ displayed — for as long as the `decoded` stamp is a **completion** stamp. On the
synchronous rungs it is. On the **native-Vulkan** rung `receive_frame` returns at *submission*
(~0.1 ms) and the stamp is taken there, so `display` is measured from submit and the GPU decode
happens **inside** it. `host+net` and `display` already tile e2e; the `decode` figure (received →
fence-complete) re-counts the GPU work `display` contains — two figures with one overlap, printed
as though they tiled.

On that rung `decode` now leaves the stage line and gets its own, carrying the two caveats a reader
needs: it is **one sample per window** there, not the p50 every other figure on that line is, and it
is already inside `display`, so adding it double-counts. The synchronous rungs are untouched.
⚠ **Deliberately not changed:** the one-sample-per-window design. A per-frame fence wait serialises
the decode pipeline (an APU's 19 ms decode capping a 5120×1440 stream at ~51 fps) and polling
quantises every sample up by a frame interval. The reporting was the defect, not the sampling.

### Host — two warnings that named the wrong subsystem

Both fired in the same 2026-08-13 field log, and both sent an investigation somewhere innocent:

- **"Client keyframe recoveries are METRONOMIC — a periodic host/display disturbance … is the
  likely cause"**, at `period_s=2.0`, naming three host subsystems. **2.0 s is the *client's*
  `FLUSH_COOLDOWN`.** The receive-backlog guard sheds a standing queue with a flush plus a keyframe
  request, rate-limited to one per cooldown, so a client that cannot sustain the stream asks for a
  keyframe at exactly that spacing for as long as it stays behind. **Perfect periodicity is the
  signature of a fixed software cooldown, not of a physical disturbance.** The host now compares
  against `punktfunk_core::client::FLUSH_COOLDOWN` itself rather than a copy of the number, so the
  two cannot drift.
- **"The audio encode thread could not keep up — captured audio was DROPPED"**, worst case
  `dropped_chunks=11251`. Not one sample anybody wanted was lost. PipeWire negotiated a 128-frame
  quantum, so the plane produces 48000/128 = 375 chunks/s and a 30 s window holds exactly 11250 —
  a 100 % drop rate at `peak_db=-120.0`, digital silence. Every one of the ten warnings straddled a
  **session boundary**, and `dropped_chunks/375` matches the seconds with *no live session* in that
  window to within a fraction of a second. The warning no longer fires for idle seconds.

### NixOS — the plugin runner was installed, running, and reported missing

🛑 **On NixOS every plugin *package* op failed with "the plugin runner isn't installed", on a box
where the runner was installed, enabled and running.** `punktfunk-host plugins status` said so, and
the console's Plugins screen still refused to install anything.

The host resolved `punktfunk-scripting` by checking FHS locations exclusively —
`/usr/bin/punktfunk-scripting`, the `/usr/lib` + `/usr/share` pair behind it, and the `~/.local`
mirror the SteamOS installer lays down. Nix installs a wrapper at `$out/bin/punktfunk-scripting` in
a **derivation of its own**, so it is neither beside the host binary nor anywhere under `/usr`, and
nothing the resolver looked at could ever match. Service ops (`enable`/`disable`/`status`) go
through systemd and were unaffected, which is what made the failure read as arbitrary: the runner
demonstrably worked, and only the half that had to *locate the executable* was blind.

Resolution now matches `punktfunk-encode-worker`'s: **`PUNKTFUNK_SCRIPTING` → beside the host
binary → `PATH` → the `/usr` layout → the `~/.local` layout.** `PATH` is the rung Nix lands on. The
`/usr` rungs are kept after it rather than dropped, because a systemd unit's `PATH` need not include
`/usr/bin`. As with the encode worker, an explicit `PUNKTFUNK_SCRIPTING` is deliberately *not*
existence-checked — a named path that is wrong should fail naming itself, not fall through to some
other runner. The "not installed" text now also names NixOS and the override, instead of pointing
every operator at `apt`.

⚠ **Packager-visible, and the other half of the fix:** the NixOS module now puts
`services.punktfunk.scripting.package` on the **host unit's** `path`. `environment.systemPackages`
only ever covered an operator's interactive shell, and the console installs plugins from *inside*
the host service — whose `PATH` is exactly that unit list. Without it the CLI would have been fixed
and the console would not. Anyone packaging the host separately wants the same property: the runner
must be on the service's `PATH`, or `PUNKTFUNK_SCRIPTING` set for it.

The `ln -s "$(command -v punktfunk-scripting)" ~/.local/bin/punktfunk-scripting` workaround is no
longer needed and can be removed.

### `/bin/true` and `/bin/false` are not portable — two tests failed on NixOS

NixOS ships only `sh` in `/bin`, so `gamelease`'s hand-off test and `pyrowave_remote`'s
handshake-rung test failed there for reasons unrelated to the code under test. Both now resolve a
real binary rather than assuming an FHS path.

### Documentation

**`docs-site/public/openapi.json` was stale again, and by the same mechanism as last release.**
v0.28.0 fixed it once (it was five releases behind at `0.21.0`); the scanner-removal regen then
updated `api/openapi.json` alone and it drifted a second time inside that same cycle. It has now
drifted a third time, across the unpair-all endpoints — the docs-site copy was still stamped
`0.27.0` and missing both collection deletes. Re-synced; the two files are byte-identical again.

⚠ **The copy is a documented manual step (`cp api/openapi.json docs-site/public/openapi.json`,
CONTRIBUTING.md) and nothing in CI enforces it.** Three drifts in two release cycles is the
argument for gating it; until something does, **treat the copy as part of regenerating, not as a
follow-up.**

### Linux — the data-plane threads finally get the priority they ask for (⚠ packager-visible)

**On every Linux host to date, `pf_frame::thread_qos`'s per-thread renice was a silent no-op** —
it needs CAP_SYS_NICE or a raised RLIMIT_NICE, no packaging channel granted either, and the host
binary can never carry a file capability (KWin identification, the 0.26.0-1 incident). So the
capture/encode and send threads ran at nice 0, and a CPU-saturating burst on the host — a fresh
game launch's shader-compile storm is the canonical one — descheduled them at will. A 2026-08-14
field log showed the result end to end: 5 ms audio datagrams leaving late enough to stutter, the
client's delay signal rising, and ABR cutting a gigabit-Ethernet session to its 5 Mbps floor with
zero packet loss — while the box carried 708 Mbps cleanly minutes later, once the storm passed.

**The renice now falls back to RealtimeKit** (`MakeThreadHighPriorityWithPID`, one blocking
system-bus call per boosted thread) — the same unprivileged broker PipeWire clients use, present
on effectively every desktop install. No capability enters the host's permitted set, so KWin
identification is untouched. Boxes with neither rtkit nor the new limit keep today's best-effort
no-op, one debug line per thread.

**The audio plane is boosted at all for the first time.** The 5 ms Opus capture→encode→send loop,
the PipeWire capture mainloop thread (its `process` callbacks run there — PipeWire's own
`module-rt` only covers data loops we don't use), and the pad-audio streamer now take the same
boost the video threads always asked for. The audio loop is `critical`: a scheduling stall there
is directly audible where a late video frame is one presentation slip.

⚠ **Packagers: a new `user@.service.d` drop-in.** rpm/deb/Arch (and the Bazzite sysext, via the
RPM) now ship `packaging/linux/50-punktfunk-nice.conf` →
`/usr/lib/systemd/system/user@.service.d/50-punktfunk-nice.conf` (`LimitNICE=-15`), so the direct
`setpriority()` also works where rtkit isn't running. It raises a session *limit*, from the next
login — nothing is reprioritized by itself. The NixOS module instead sets
`security.rtkit.enable = lib.mkDefault true` (rtkit is not a given there). It remains true that
**no channel may ever grant the host binary a file capability** — this change is the sanctioned
route to the same end.

---

## v0.28.0

180 commits since v0.27.0.

### Versions

| | v0.27.0 | v0.28.0 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged |
| C ABI | 18 | **19** | `punktfunk_connection_note_frame_index_ex` + `punktfunk_reanchor_gate_arm_expecting_drops` **added**; nothing removed, nothing widened |
| Rust edition | 2021 | **2024** | the whole tree bar four vendored crates |
| MSRV (`rust-version`) | 1.82 | **1.85** | the *declared floor* only — the pinned toolchain is unchanged |
| Workspace crate dirs | 27 | **27** | unchanged (39 members; two `tools/` crates still deliberately *excluded*) |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3) |
| Windows virtual-gamepad channel | 3 | **3** | unchanged |
| Plugin index schema | 1 | **1** | unchanged |
| `api/openapi.json` | 0.25.0 | **0.27.0** | the management API **did** change this release (below); the file was regenerated mid-cycle, so it carries the then-current stamp, not `0.28.0` |
| gamescope patch level (`+pfhdrN`) | 5 | **6** | 7 patches → 8 (`GAMESCOPE_NO_FOCUS`); no new capability |
| `@punktfunk/host` (SDK) | 0.1.4 | **0.1.4** | unchanged |
| `@punktfunk/plugin-kit` | 0.4.0 | **0.4.1** | publishes the `icon` field |

⚠ **`crates/pf-driver-proto` changed again**, as it did in v0.27.0 — but *not* in its contract. The
wire bytes, `PROTOCOL_VERSION` (6) and `MIN_DRIVER_PROTOCOL_VERSION` (3) are all untouched; what
moved is the manifest (`edition`/`rust-version` now inherit from the workspace) and one test that
was reading a `[u8; 40]` through `bytemuck::from_bytes` — an alignment assumption a favourable
stack slot had been hiding, and the kind of thing Miri exists to catch (below). If you ship the
driver or the gamepad channel, this release needs no re-integration.

⚠ **`api/openapi.json` is still not gated by CI** — nothing regenerates or diffs it in a workflow.
A unit test (`openapi_document_is_complete_and_checked_in`) does compare the checked-in copy against
the served document, with `info.version` normalized on both sides, so the *surface* is protected
even though the stamp drifts. The docs-site copy is a plain file copy and was **not** protected:
see the note under **Documentation** below.

### ⚠ Breaking changes

**None on the wire, and none that break an embedder at runtime.** Wire protocol 2 is unchanged, so
existing pairings and every shipped client keep working; the C ABI moves by addition only. What
follows changes what the **host itself does**, how you **build**, and what a **stock package does by
default**.

- 🛑 **The host no longer scans any launcher itself — the six built-in library scanners are
  deleted and replaced by plugins.** This is the only change here that can leave a working install
  visibly emptier: **a host with no library plugins installed has an empty grid.** Full detail and
  the (deliberately absent) migration below.
- **Rust edition 2024, MSRV floor 1.85.** If you vendor or patch any workspace crate, your toolchain
  must be ≥ 1.85. Our pinned toolchain did not move — only the declared floor.
- **Building from source now needs a working C compiler**, because `aws-lc-sys` compiles AWS-LC.
  No CMake, Go or NASM for the default (non-FIPS) build. Detail under the TLS section below.
- **GameStream is opt-in on every route.** A packaged host that served Moonlight by default becomes
  native-only until the operator sets `PUNKTFUNK_GAMESTREAM=1`. Full detail below.
- **No punktfunk process holds REALTIME GPU priority any more.** Both levers (the driver's
  `IddCxSetRealtimeGPUPriority` raise and the host's `HIGH → REALTIME` auto-upgrade) default OFF;
  the ladders that re-enable them are new opt-ins. This is a field-convicted stall fix, below.
- **The shipped Bazzite `host.env` template no longer pins `PUNKTFUNK_GAMESCOPE_ATTACH=1`.** If you
  copied it verbatim — which the docs told you to — Game Mode was mirroring the box's screen. Below.

### The six built-in library scanners are gone — every game source is a plugin (⚠ operator-visible)

The host no longer scans any launcher itself. `library/{steam,epic,gog,heroic,lutris,xbox}.rs` and
the `scanner_defs()` table are deleted; `GET /library/scanners` now lists exactly what the operator
has installed, and every row reports `origin: "plugin"`. This is M6/WP6.4, the end of the migration
whose bridge half shipped in v0.26.0 — the plugins have been published and index-pinned since
2026-08-08.

**A host with no library plugins installed has an empty grid.** That is the upgrade note: the
console's Library page offers one-click install per source (the D9 nudge, still there and still
never auto-installing), and nothing about a title changes when its plugin takes over.

Why that last part is true, and why this was safe to do as a deletion rather than a rewrite: a
plugin **claims** its store (D2), and a claimed entry surfaces under the deterministic
`<store>:<external_id>` id the scanner used to produce. Entry ids, GameStream FNV-1a app ids,
client-side art caches, Moonlight pins, the operator's per-source toggles and their per-entry hides
are all keyed on that id and none of them move. `library-scanners.json` keeps its name, its shape
and its contents — an operator who had `steam` switched off still has it switched off, with no
migration step.

What survives the scanners, deliberately:

- **`launch.rs` in full.** Launch is host-owned by design D1 — a plugin publishes a validated
  *value* and the host builds the command — so every typed kind (`steam_appid`, `steam_ui`,
  `launcher_ui`, `epic`, `gog`, `aumid`, `xbox`, `lutris_id`, `playnite`) stays exactly as it was.
  `xbox_pfn()` moved here from the deleted `xbox.rs`: resolving a package Identity to its
  PackageFamilyName needs `AppRepository` enumeration, which is readable by the host (LocalSystem)
  and denied to the plugin runner (LocalService), and that measured asymmetry is the entire reason
  the `xbox` launch kind exists.
- **`SourceOrigin::Builtin`.** No host build emits it, but the web console ships as its own package
  and is expected to drive an N-1 host that still does, so the variant stays in the schema and the
  console keeps its `builtin` handling.
- **The store-label table.** Six ids keep their display names (`steam` → "Steam", …) so a source row
  does not rename itself to a bare id the day its plugin takes over.

Removed with them: the background cover-art warmer and its on-disk cache (they existed only for the
GOG and Xbox scanners, the two sources that had to ask a network catalog what a cover was — a
plugin resolves art while it scans), the legacy `steam:` branch of the art proxy, and the
`GameMeta::pc()` helper. **The host now makes no outbound HTTP request to build a library at all.**

⚠ **Dependency drop (packager-visible):** `rusqlite` (with its bundled, `cc`-compiled SQLite) and
`roxmltree` leave the host's dependency graph — they had no other users. `winreg` stays: `launch.rs`,
`procscan/windows.rs` and the two `audio/windows/` modules still need it. `base64`/`ureq` stay, as
the M6 plan predicted.

A stale `library-art-cache.json` from an older host is ignored, not migrated.

### GameStream is now opt-in on EVERY route (⚠ packager-visible default change)

The secure native-only host is the default everywhere; the Moonlight-compat planes (plain-HTTP
pairing + the legacy GCM path, security-review #5/#9) are enabled only by an explicit choice:

- **The shipped systemd user unit** (`scripts/punktfunk-host.service`, installed by deb/RPM/Arch/
  sysext) runs bare `serve` — `--gamestream` is no longer baked into `ExecStart`. Opt in via the
  new **`PUNKTFUNK_GAMESTREAM=1`** knob in `host.env` (pf-host-config; equivalent to the flag —
  either source enables), so no unit editing survives-upgrades dance is needed.
  ⚠ **Upgrade note:** a packaged host that served Moonlight by default becomes native-only until
  the operator sets the knob (a hand-made `ExecStart` drop-in keeps winning as before).
- **NixOS module**: `services.punktfunk.host.gamestream` default flipped `true` → `false`
  (module-check gained a "default is native-only" assertion); enabling it still opens the
  GameStream firewall ports.
- **Steam Deck installer**: `--gamestream` opts in (was on-by-default with `--no-gamestream`;
  the old flag is still accepted as explicit-off).
- Windows was already opt-in (unchecked installer task) and is unchanged.

### TLS moved to aws-lc-rs, with post-quantum key exchange (⚠ build-visible for packagers/embedders)

The rustls backend across the whole workspace — host, tray, clients and `punktfunk-core` — is now
**aws-lc-rs** instead of `ring`, which enables rustls's `prefer-post-quantum`: every TLS 1.3
handshake (management API, the native `punktfunk/1` control plane, QUIC) now offers the
**X25519MLKEM768** hybrid key exchange first. Ring has no ML-KEM, which is why the backend had to
move. This is negotiation-only and additive — the classical curves stay in the list, so any client
that does not implement ML-KEM connects exactly as before, and no wire format, ABI or pairing
record changes. The session AEAD (AES-128-GCM / ChaCha20-Poly1305) is a separate mechanism and is
untouched.

⚠ **Building from source now needs a working C compiler**, because `aws-lc-sys` compiles AWS-LC.
No CMake, Go, or NASM is required for the default (non-FIPS) build — on Windows x86_64 rustls turns
on `aws-lc-rs/prebuilt-nasm`, so no NASM has to be installed. If you add a crate that depends on
`aws-lc-rs` *directly*, name `features = ["prebuilt-nasm"]` on it: a package selection that pulls
`aws-lc-rs` without also enabling rustls's `aws_lc_rs` feature otherwise fails on Windows.

`punktfunk-core` gains an off-by-default **`ureq-tls`** feature (`tls::ureq_agent`) that builds a
blocking HTTP agent around a caller-supplied `rustls::ClientConfig` — the only way to install the
fingerprint-pinning verifier, since ureq's own `TlsConfig` has no hook for one. The desktop client
and the tray enable it; the Apple/Android cdylib embedders do not, and pull no HTTP stack.

**`ring` is gone from the tree entirely** — aws-lc-rs is now the only crypto backend on every
target we ship. Getting there needed the `ureq 2 → 3` upgrade in the same change, because ureq 2
named `rustls/ring` inside its own dependency declaration where no dependent could switch it off.
ureq 3 declares rustls with `default-features = false` and picks no backend, so the choice is
finally ours. ⚠ Spell that dependency `features = ["rustls-no-provider", "rustls-webpki-roots"]`:
ureq 3's convenience `rustls` feature pulls `_ring` and would quietly restore the second backend.

The ureq upgrade is otherwise internal, but two behaviours are worth knowing. Response size caps
are now enforced by the body reader, so an over-cap response is an **error** instead of ureq 2's
silent truncation (which used to surface as a confusing signature failure). And a fingerprint
mismatch is now matched on ureq 3's typed `Error::Rustls(..)` rather than by sniffing a substring
out of a transport error message — the old test could also fire on unrelated certificate errors.
Conditional requests are unchanged: ureq 3 still returns 304 as `Ok`, only 4xx/5xx become `Err`.

**Embedders of `punktfunk-core` that build their own rustls configs** should still call
`punktfunk_core::tls::install_default_provider()` at startup, or use `builder_with_provider`. With
one backend present rustls can infer it, so this is now insurance rather than a requirement — but
it is what stops a future second backend from turning config construction into a panic.

### The ENet control port now exists only while a pairing does (rust-safety WP0)

`rusty_enet` — a c2rust-style transpile of C ENet, and the host's only pre-auth-reachable unsafe
surface — no longer listens unconditionally: UDP 47999 binds when the paired-client list becomes
non-empty and is torn down when the last pairing is removed (a live client gets the same
TERMINATION+disconnect farewell as a host-side session end). Pairing itself is HTTPS on nvhttp and
never touches the port, so a never-paired `--gamestream` host exposes no ENet at all. En route:
the management API's unpair endpoint never persisted (`save_paired` was missing), so an unpair
lasted only until the next restart — fixed. `rusty_enet` is now pinned `=0.4.0`.

**Unpair is now a complete revocation, on both planes.** Beyond the persistence fix above, an
unpair used to leave the revoked client's LIVE session streaming until the client chose to
leave. Now: unpairing a GameStream client whose certificate owns the active launch ends that
session (the client gets the standard TERMINATION+disconnect, and unpair-all still closes the
ENet port); unpairing a native client deliberately stops its live punktfunk/1 session(s)
(matched by certificate fingerprint — anonymous/TOFU sessions are unaffected, they have no
pairing to revoke). The unpair endpoint's long-standing docstring caveat ("removes the client
from the listing without severing its ability to reconnect") is retired: TLS-level handshakes
still complete by design, but authorization is per-request and a live session no longer
survives its own revocation.

### GameStream is now a cargo feature (compile-time isolation — packager-visible)

The Moonlight-compat planes (nvhttp pairing, RTSP, the ENet control stream, `_nvstream` mDNS,
the compat media path) are gated behind a new **`gamestream` cargo feature — default ON**, so
every stock package is behaviorally identical (GameStream stays runtime-opt-in via
`--gamestream` / `PUNKTFUNK_GAMESTREAM`). Building with
`--no-default-features --features pyrowave` produces the **hardened native-only host**:

- **no `rusty_enet`** — the c2rust-transpiled C ENet stack (158 unsafe sites) is absent from
  the binary, provably (`cargo tree -i rusty_enet` finds nothing; CI asserts it);
- **no `rsa`** — the native planes run on the P-256 identity (above), and the legacy-identity
  fallback is a pem-only read (rustls/ring serves an existing RSA cert without the crate), so
  the accepted Marvin advisory (RUSTSEC-2023-0071) no longer applies to native-only builds;
- ~6,700 lines of Moonlight protocol code gone; `serve --gamestream` (or the env knob) against
  such a binary **refuses to start** with a clear error rather than serving less than asked;
- the native-only management API (and its OpenAPI document) has no GameStream PIN endpoints
  (`/api/v1/pair`, `/api/v1/pair/pin`); everything else — including the paired-client list and
  unpair — is identical, so consoles work unchanged.

The checked-in `api/openapi.json` remains the default-features document.

### The identity split — the native planes get their own (P-256) host identity

One RSA-2048 identity historically served every plane, because Moonlight mandates RSA and the
planes grew out of the GameStream host. The native punktfunk/1 QUIC plane and the management API
now share a separate **ECDSA P-256** identity (`native-cert.pem`/`native-key.pem`): generated by
rcgen on the workspace's aws-lc-rs backend, browser-compatible (Ed25519 server certs are not),
carrying real SANs
(localhost, loopback, the machine hostname — the legacy cert had none), and free of the accepted
`rsa`-crate Marvin advisory. The GameStream plane keeps the RSA identity untouched.

**Migration is pin-preserving by construction**: clients TOFU-pin the leaf-cert SHA-256 at
pairing and use that one pin for both QUIC and the mgmt/library API, so the new identity is
adopted **only when the native trust store is empty** (fresh installs, or after an explicit
unpair-all + restart). An upgraded host with live native pairings keeps presenting the legacy
RSA cert those clients pinned, and logs the migration path. Fingerprint pinning is
algorithm-agnostic, so existing shipped clients pair against P-256 hosts unchanged.

Follow-the-identity consumers updated in-tree: the tray's loopback pin and the plugin SDK's
mgmt CA now prefer `native-cert.pem` (falling back to `cert.pem`), and the Windows runner ACL
grant covers both. ⚠ A plugin bundling an **older** `@punktfunk/host` SDK on a **fresh**
(P-256) host trusts the wrong cert — set `PUNKTFUNK_MGMT_CA=<config>/native-cert.pem` in its
environment or rebuild against the current SDK.

⚠ **It is ECDSA P-256, not Ed25519 — deliberately.** rcgen can generate either, and Ed25519 would
be the obvious modern pick, but **no mainstream browser accepts an Ed25519 server certificate** and
an operator opens `/api/docs` in one. P-256 is the strongest curve that keeps the management API
reachable from a browser.

#### 🗓 Deprecation: the legacy-identity fallback goes away on **1 October 2026**

The fallback in `load_or_adopt` — "an upgraded host with live native pairings keeps presenting the
legacy RSA cert those clients pinned" — is a **migration aid, not a permanent branch**. From
**2026-10-01** the host stops taking it: a host that still holds only `cert.pem`/`key.pem` will mint
the P-256 identity and its native clients will have to re-pair once.

**Scope, precisely** — this affects the **native punktfunk/1 plane and the management API only**:

- **The GameStream/Moonlight plane is NOT deprecated and keeps its RSA identity permanently.**
  Moonlight mandates RSA and its pairing hashes bind the cert's X.509 signature bytes, so that
  identity cannot move without breaking every Moonlight client. Nothing about that changes on any
  date.
- Operators who want the split **today** need no new release: unpair all native clients, restart the
  host, re-pair. The host already logs exactly this.
- Fresh installs since v0.28.0 are already on P-256 and are unaffected.

⚠ **This date is a published commitment**, tracked as
[#201](https://git.unom.io/unom/punktfunk/issues/201) (due 2026-10-01), which carries the arm to
delete, the three identity-following consumers to re-check, and the test that has to invert. Without
it the notes would have promised something that silently never happens — the same shape as the
v0.22.3 notes describing a feature that release never contained.

### Memory-safety, compiler-enforced (embedder-visible lint tightening)

`punktfunk-core` now carries `#![deny(unsafe_code)]` crate-wide: everything that parses network
bytes is safe Rust by compiler-enforced invariant. The documented `#![allow]` carve-outs are the
client surface (`abi`, `client`) and the platform syscall-batching shims under `transport`
(`udp/{apple,linux,windows}`, `qos_windows`) — none of which interpret attacker bytes. In
`punktfunk-host`, the modules a secure-default host exposes (`native`, `native_pairing`, `mgmt`,
`mgmt_token`, `discovery`, `wol`) are `#[forbid(unsafe_code)]`. If you embed `punktfunk-core` and
patch it, new unsafe outside the carve-outs is now a compile error.

### NixOS + KDE — session detection, the other half

🛑 **v0.27.0's NixOS session-detection fix did not reach a stock NixOS + Plasma 6 box.** It resolved
the nixpkgs wrapper decoration through `/proc/<pid>/exe` (below) — and on that exact box the kernel
refuses to let us read that link. Reading `/proc/<pid>/exe` is not gated on owning the process: it
goes through `cap_ptrace_access_check`, which requires the reader's effective set to be a superset
of the target's **permitted** set. NixOS's own Plasma module ships
`security.wrappers.kwin_wayland = { capabilities = "cap_sys_nice+ep"; }`, so KWin holds a capability
and the host — which must stay uncapped, because a capability is exactly what makes it
unidentifiable to KWin (v0.27.0, above) — gets `EACCES`. The two traps compose: the name *needs*
`exe` because nixpkgs wrapped the binary, and `exe` is *denied* because NixOS capped it. Detection
went straight back to `ActiveKind::None`, `wayland` to `-`, and every connect to
`no usable compositor`. It presents identically to the v0.27.0 bug, which is why a box that had been
worked around with a decoy process broke again the moment the decoy was removed.

Name resolution now falls through to `argv[0]` (`/proc/<pid>/cmdline`) when the kernel refuses `exe`.
That reads correctly for the same reason `ps` does: make-wrapper's wrapper `exec -a "$0"`s the hidden
binary, so `argv[0]` survives the decoration `comm` does not. Measured on Linux 6.x against a capped
target, for a file capability and for the ambient form `security.wrappers` uses, identically: the
`/proc/<pid>` directory keeps its real owner (so the uid filter was never the problem), `comm` and
`cmdline` stay readable, and only `exe` fails. `argv[0]` is consulted **last** and never overrides a
readable `exe` — it is the process's own claim about itself, and a same-uid process can set it to
anything; the worst a spoof achieves is aiming detection at a backend that then fails its own
availability probe. The `comm` fast path is still one read for every ordinary distro.

Also reached by the same rung: `gamescope` carries `cap_sys_nice` on a number of distros, so a
*wrapped and capped* gamescope was equally invisible to the foreign-gamescope probe.

### Game Mode on Nobara — the WSI opt-out never reached the games

🛑 **v0.27.0's fix for the distro Vulkan WSI layer was clobbered by the session script, so games ran
on a black screen** while the host's own log claimed the layer had been disabled. Steam Big Picture
came up, showed the right mode, showed the perf overlay — and then every game played sound and took
input over a black picture, with no error on either side.

The layer (`VkLayer_FROG_gamescope_wsi`) ships with the *distro's* gamescope and speaks its
`gamescope_swapchain` protocol; ours disagrees, so the compositor rejects the client's
`swapchain_feedback` and kills it. v0.27.0 turned the layer off with `ENABLE_GAMESCOPE_WSI=0` on the
session unit. `gamescope-session-plus` then runs an unconditional `export ENABLE_GAMESCOPE_WSI=1`
near the top of the script — before it launches anything — so the opt-out survived exactly as long
as it took the script to start, and every process the session spawned got the layer back. Nothing
looked wrong because the casualty is Vulkan clients specifically: Steam's own UI is not one.

The opt-out is now `DISABLE_GAMESCOPE_WSI=1` as well. The Vulkan loader reads an implicit layer's
two manifest knobs in a fixed order: `enable_environment` must equal `"1"` to switch the layer on,
and `disable_environment` is then consulted last and wins on **presence alone**, at any value. The
session script never mentions that second variable, so it is the one that survives. Both spellings
go out, on the transient unit and on the box's own session drop-in.

### punktfunk-gamescope `+pfhdr6` — a NO_FOCUS window can no longer steal the composite

🛑 **A mapped-but-unpainted window carrying `GAMESCOPE_NO_FOCUS=1` could win gamescope's focus
selection and turn the composite — and the stream fed from it — black while every health signal
stayed green.** Bazzite's hhd-ui (Handheld Daemon overlay) sets that atom once at init, stamps
Steam's appid, and crash-loops under a headless takeover; each respawn remapped a fullscreen black
window that steamcompmgr then chose over Big Picture (observed on a Bazzite box: client stats
happily decoding 60 fps at 0.1 Mb/s of black; killing hhd-ui restored the picture instantly). No
gamescope — upstream or Bazzite's fork — ever consumed the atom; its setters (hhd-ui, MangoHud)
show and hide via the `STEAM_OVERLAY` protocol and rely on never being focusable. Patch 0008 wires
`GAMESCOPE_NO_FOCUS` exactly like `GAMESCOPE_EXTERNAL_OVERLAY` (read at map, PropertyNotify-tracked,
skipped by both focus-candidate collectors) without touching compositing or `appID`. Banner
`+pfhdr5` → `+pfhdr6`; no new capability — the bump is so a field box's banner tells the two
behaviors apart.

### Linux capture — the truncated first attempt no longer latches sticky downgrades

🛑 **The pipeline retry loop's deliberately short (2.5 s) first-frame attempt could permanently
downgrade the whole host process.** On expiry, the portal capturer's timeout diagnosis latched
whichever offer it implicated — HDR capture off (per source), the raw-dmabuf offer off, the
EGL→CUDA offer off — as if the compositor had refused it, when the budget was truncated by design
and a gamescope cold start routinely needs longer before delivering anything. One lost race at
connect then pinned every later session to SDR and/or CPU capture until the host restarted. The
truncated attempt is now declared provisional end to end
(`Capturer::next_frame_within_provisional`): its expiry names the same suspect in the error text
but latches nothing; only the full-length attempts that follow hand down negotiation verdicts. The
classification is a pure function with tests
(`pf_capture::linux::first_frame_timeout_tests`).

### Windows host — an idle box can sleep again (virtual-mic stream idle-stop)

🛑 **Installing the host blocked system sleep forever, client connected or not.** The
host-lifetime mic pump kept a WASAPI render stream RUNNING on the virtual-mic device
(typically the Steam Streaming Microphone), writing silence 24/7 — and any running stream makes
the Windows audio stack hold a kernel power request ("An audio stream is currently in use" in
`powercfg /requests`, attributed to that device) that vetoes sleep. The render loop now stops
the stream (`IAudioClient::Stop`; the client stays initialized and the mic *endpoint* keeps
existing for apps to bind) after 10 s of silence-only output and resumes on the next mic frame
within one device period — below the jitter buffer's prime depth, so nothing is audible.
Streaming sessions still hold the box awake through their own `PowerRequest` assertions, as
before. New knob: `PUNKTFUNK_MIC_ALWAYS_ON=1` restores the old always-running stream in case a
third-party virtual audio driver misbehaves while its render side is paused.

### Windows host — audio no longer costs local-game frame time

🛑 **The host could tank a locally-played game's frame lows** (field-reported 2026-08-12:
Helldivers 2 at 1% lows of 2–5 FPS, cured by uninstalling). Two mechanisms, both fixed:

- **The minted-endpoint retry storm.** The virtual-mic resolve ran a FULL provisioning pass on
  every reopen with no cooldown, no in-flight guard, and no give-up — and the pass reached
  `UpdateDriverForPlugAndPlayDevicesW` even over an already-existing devnode. On a box where
  minting cannot converge, the pump's reopen backoff (capped 60 s) turned that into a SetupAPI
  sweep + PnP driver re-bind + default-device writes roughly once a minute, forever — each
  raising the system-wide device-change broadcast games service by rebuilding their audio
  graphs. Provisioning now short-circuits to a no-PnP fast path while the minted devices are
  healthy, waits on an in-flight pass instead of racing a second one, honours the 60 s retry
  cooldown from the blocking path too, and stops for the host lifetime after five unlatched
  passes (a service restart re-arms minting).
- **Session tuning never reverted.** The first streaming session put the whole host process at
  HIGH priority class with a 1 ms global timer (`timeBeginPeriod`) and DWM MMCSS, documented as
  "reverts at process exit" — but the host is a 24/7 service, so after one stream it competed
  at HIGH priority against whatever the user played locally, forever. The process-wide tuning
  is now refcounted across the hot stream threads and reverts when the last one exits
  (= session teardown), the same lifetime the per-thread MMCSS effects already ride.

### Debian 13 is a supported target, and `punktfunk-gamescope` reaches apt for the first time

🛑 **The `punktfunk-gamescope` .deb had never been published — not once, in any release.** It was
built inside the host job's Ubuntu 24.04 image, where it cannot build: our pin vendors wlroots
0.19.3, which floors `wayland-server` at 1.23.1, and noble ships 1.22.0 (it also has no
`libxcb-errors-dev` and only libdisplay-info 0.1.1). Every rung of that path was a `::warning::`
returning 0, and the one hard gate ran last by design so good artifacts still shipped — so
**v0.26.0 and v0.27.0 both released with the package missing** while the release notes and
docs-site told Debian/Ubuntu users to `apt install` it. The same tag shipped it fine for Arch,
Fedora 44 and Bazzite; apt was the only platform affected.

It now has its own job on **Debian 13** (`ci/gamescope-trixie.Dockerfile`), the oldest apt base the
tree configures on. One package serves Debian 13 **and** Ubuntu 26.04 — verified by installing and
running it on both — because the build additionally vendors libdisplay-info
(`build-punktfunk-gamescope.sh --extra-fallback libdisplay-info`, opt-in so the Arch/Fedora/nix
outputs are unchanged): linked against the distro copy it would demand `libdisplay-info2` on trixie,
which Ubuntu 26.04 does not have (it carries `libdisplay-info3`). **Ubuntu 24.04 gets no gamescope
package** — its wayland is too old to run one, however it is built.

⭐ **Debian 13 is now a documented, CI-tested host target** ([docs](https://docs.punktfunk.unom.io/docs/debian)).
It required no packaging change: the host .deb's glibc-2.39 floor and bundled FFmpeg already made
it installable, and it had been working for a long time while docs-site said Debian was unsupported
and unverified. The desktop **client** remains Ubuntu-26.04-only (built there, floors at
`libc6 >= 2.43`; Debian 13 has 2.41).

⚠ **Cinnamon (Linux Mint, LMDE) cannot host a virtual display**, and compositor detection now says
so instead of advising a `PUNKTFUNK_COMPOSITOR` value that cannot help. Muffin forked from Mutter
3.36: `org.cinnamon.Muffin.ScreenCast` has only `RecordMonitor`/`RecordWindow`, never
`RecordVirtual`, and `xdg-desktop-portal-xapp` implements no ScreenCast at all. The error names the
route that does work on those boxes — a headless gamescope, which needs no desktop compositor.

New CI job **`smoke-install`** installs every published package from the registry in pristine
`ubuntu:24.04`, `ubuntu:26.04` and `debian:trixie` images and asserts the version served is the one
the run just built. Nothing in `deb.yml` had ever installed a package it produced, which is how
both facts above survived for so long.

### 🛑 The six built-in library scanners become plugins (M6/WP6.4 — breaking)

The host no longer scans any launcher. `library/{steam,epic,gog,heroic,lutris,xbox}.rs` and the
`scanner_defs()` table are **gone**; `GET /library/scanners` now lists exactly what the operator
installed, every row `origin: "plugin"`. This ends the migration whose bridge half shipped in
v0.26.0 — the plugins have been published and index-pinned since 2026-08-08, so the replacement has
been in the field for the whole bridge window.

⚠ **The upgrade note is the whole of it: a host with no library plugins installed has an empty
grid.** The console's one-click install per source is unchanged and still never auto-installs.

⭐ **There is no migration, by construction, and that is why this could be a deletion rather than a
rewrite.** A plugin *claims* its store (D2), and a claimed entry surfaces under the same
deterministic `<store>:<external_id>` id the scanner used to produce. Entry ids, GameStream FNV-1a
app ids, client art caches, Moonlight pins, the per-source toggles and the per-entry hides all key
on that id and **none of them move**. `library-scanners.json` keeps its name, shape and contents —
an operator who had `steam` off still has it off.

Kept deliberately:

- **`launch.rs` in full.** Launch is host-owned by design (D1): a plugin publishes a validated
  value, the host builds the command, so every typed kind survives. `xbox_pfn()` **moved here** out
  of the deleted `xbox.rs` — resolving a package Identity to its PackageFamilyName needs
  `AppRepository` enumeration, readable by the host (LocalSystem) and **denied to the plugin runner**
  (LocalService). That measured asymmetry is the entire reason the `xbox` launch kind exists, so the
  resolver is launch vocabulary, not scanner vocabulary.
- **`SourceOrigin::Builtin`.** No host build emits it any more, but the console ships as its own
  package and drives an N-1 host that still does, so the variant stays in the schema.
- **A store-label table**, so a source row does not rename itself from "Steam" to `steam` the day
  its plugin takes over.

Removed with the scanners: the background cover-art warmer and its on-disk cache (they existed only
for GOG and Xbox, the two sources that had to ask a network catalog what a cover was — a plugin
resolves art while it scans), the legacy `steam:` branch of the art proxy, and `GameMeta::pc()`.

### Mutter monitor rebuilds are serialized end to end — the two-client chain no longer kills GNOME

🛑 **Chaining two clients through a kept (keep-alive) Mutter display segfaulted gnome-shell in
`meta_monitor_manager_rebuild` (libmutter-18) and took the whole desktop down**; every later session
then failed `RemoteDesktop.CreateSession: ServiceUnknown` until GDM restarted, so the client just sat
black. ⭐ **A/B'd on .21 during this release's validation: byte-identical on the released 0.27.0 and
on the 0.28.0 RC — it was never a regression, the trigger had been there all along.**

`TOPOLOGY_LOCK` already serialized every topology-mutating D-Bus call, but two gaps let Mutter's
*rebuilds* overlap:

- **Teardown was fire-and-forget.** `StopGuard::drop` set a flag and returned; the session thread
  only noticed on its ≤200 ms park tick. The dead-reuse path (reused kept display dead on first
  frame → `mark_failed` → re-create) therefore issued its fresh `RecordVirtual` with the doomed
  monitor's removal still pending — the fresh session could even win the lock *before* the old
  thread had woken to take it, adding a monitor while the dead one still stood. The drop now waits
  (bounded, 20 s) for the session thread to finish.
- **The lock was released while the shell was still rebuilding.** `Stop` / `RecordVirtual` /
  `ApplyMonitorsConfig` all return mid-rebuild, and an `APPLY_TEMPORARY` config auto-reverts
  asynchronously on top. Every locked mutation now ends with `settle_topology()` — poll
  `GetCurrentState` until a removed connector is actually gone and the config serial holds still
  across two consecutive reads — before the guard drops. Bounded at 4 s and best-effort (a read
  error means the shell is gone; a hotplug storm must not park sessions), degrading to exactly the
  old behaviour.

Cost when Mutter is already quiet: one confirming read plus one 150 ms recheck per setup/teardown.

### KWin ≤60 Hz — the virtual output's real size is finally read back

🛑 **A 4K60 GameStream session captured 1920×1080.** `create()` asked KWin for 3840×2160, KWin built
something else, and nothing compared the two: only the >60 Hz arm read anything back, and it gets
that for free because it installs a custom mode. The ≤60 Hz arm installs nothing, which is exactly
why it never noticed.

⚠ **The line that should have caught it was the one that hid it.** `spawn_vout` returns a node id,
never a size, so `tracing::info!(node_id, width, height, "KWin virtual output ready")` was echoing
the **request** — the field log stated 3840×2160 while the output was 1080p, and the first pass at
diagnosing this was done against that number. It now logs `requested_w`/`requested_h` with the
readback beneath it.

### Apple/Android audio — the de-prime fuse counted callbacks, not time

🛑 **An iPad gave up on its audio ring three times sooner than a Mac**, which is the residual Apple
jitter that survived both the PLC fix (#82) and the jitter-policy fix (#111).
`JitterTuning::deprime_after` counted **callbacks**, and a callback is not a unit of time: the same
`4` was ~44 ms of starvation slack on a Mac's ~11 ms quantum and **20 ms on iOS**, whose session asks
for a short IO buffer — the shortest fuse of any client, on the one with the burstiest transport. A
100 ms Wi-Fi delivery stall therefore de-primed the Apple ring on every bunching cycle while the
identical policy rode it out everywhere else. It is now **`deprime_ms`**, measured in starved audio,
with a `MIN_DEPRIME_CALLBACKS` floor so a large-quantum device keeps real hysteresis instead of
de-priming on the first short read. ⚠ **Android was latently exposed too** — AAudio's low-latency
burst is ~4–5 ms, so its `5` was also ~20 ms.

Measured by driving the real policy through a simulated link (100 ms stall / 5 s, −30 ppm, 10 min)
at a 5 ms quantum: **120 audible gaps and 690 ms of dead air before, 2 gaps and 60 ms after.**

### Console — "Update all" on the plugins screen

The Installed tab could only update one plugin at a time, one dialog and one watched job each. The
bulk action now sits beside the list it acts on, plus a count badge on the Installed tab trigger
(Browse is the tab the page opens on, and a control nobody passes is a control nobody finds).
⚠ **The host takes ONE package operation at a time** — 409 otherwise, because bun operations share a
lockfile and a `node_modules` tree — so this is a queue the console works through job by job, driven
by each job settling rather than by a timer, carrying its own copy of what is left.

### Android — the in-stream mic control leaves the stream overlay

The mic element sat in the top-right of every stream that opened a capture (a standing button on
touch, a Muted badge on TV). It is gone for now; the on-screen overlay UI being built will carry
mute as one of its controls. **Mute itself is untouched** — `micRunning`/`micMuted`/`setMicMuted`
still back the Select + Y chord, which is now the whole of the control, and `MicChordHint` is its
only on-screen feedback.

### ⚠ Flatpak — the currency wave's one loose end

🛑 **Every flatpak leg died after #193.** The dependency currency wave took skia-safe/skia-bindings
0.87.0 → 0.99.0 in `crates/pf-console-ui/Cargo.toml`, but `packaging/flatpak/io.unom.Punktfunk.yml`
still pinned the **0.87.0** prebuilt archive, so the build failed with
`no variant … named 'Default' found for enum 'SkPathFillType'` inside
`skia-bindings-0.99.0/src/defaults.rs`. Nothing in that message points at the manifest, so it reads
like a crate bug — it is not: `SKIA_BINARIES_URL: file://…` makes skia-bindings unpack the pinned
tarball verbatim, **including its `bindings.rs`**. Archive pinned to 0.99.0.
⇒ **If you bump `skia-safe`, bump the flatpak archive in the same commit.**

### Rust edition 2024 across the tree (MSRV floor 1.85)

The whole main workspace and `pf-vkhdr-layer` move to **edition 2024**; `[workspace.package]`
declares `edition = "2024"` and `rust-version = "1.85"`. The pinned toolchain did not move — only
the declared floor — but if you vendor or patch a workspace crate, 1.85 is now the minimum.

This is the safety half of the rust-safety programme's §8.4, not a tidy-up: in edition 2024
`std::env::set_var`/`remove_var` are **`unsafe fn`**, which converts an entire bug class from
invisible to counted. The environ data race the programme found the hard way lived in a file
containing zero occurrences of the word `unsafe`; every one of the 20 files that mutate the
environment now carries an `unsafe` block with a SAFETY comment naming the actual serialization
argument (a named lock, or a `--test-threads=1` contract, or single-threaded startup).

What a downstream integrator sees:

- The 13 crates that pinned `edition = "2021"` **literally** now inherit from the workspace. A root
  bump alone would have reached only the `edition.workspace = true` crates and left `pf-encode`,
  `pf-capture`, `pf-inject` and friends on 2021 while reading as complete.
- 148 `#[no_mangle]` → `#[unsafe(no_mangle)]` (83 of them in `abi.rs`), and 12 bare `extern` blocks
  → `unsafe extern`. Done textually across **all** `cfg` branches, because 44% of the host's unsafe
  is Windows-only and a one-platform `cargo fix` silently misses it.
- `gen` is a reserved keyword in 2024, so `pf-vdisplay`'s generation stamps and the WinUI shell's
  animation counters rename `gen` → `generation`. **Internal identifiers only — no serde field, no
  wire name and no API surface changed.**
- The four **vendored** crates (`fec-rs`, `cros-codecs`, `usbip-sim`, the patched `ndk`) stay on
  2021 deliberately: upstream code stays pristine.

### No punktfunk process holds REALTIME GPU priority by default (⚠ default change)

🛑 **Both of our REALTIME GPU-scheduling levers were convicted of *generating* the metronomic
capture-stall class the stall program has chased for weeks** — compose-silence holes of 150–800 ms
in which ETW shows no process presenting while the GPU stays responsive. From the RX 9070 XT field
A/B: the virtual-display driver's `IddCxSetRealtimeGPUPriority` raise beat at ~1.75–1.78 s, and the
host's `HIGH → REALTIME` auto-upgrade beat at ~3.58 s in the sessions where it promoted. Disabling
each removed its own metronome; pinning both left the stall rate at the clean-run baseline.

Neither period matches **any** punktfunk clock — the full periodic-actor census (driver drain,
16 ms `E_PENDING` wait, 33 ms cursor poll, 3 s watchdog; host descriptor poll, VRAM gate, exclusive
re-assert, pinger, stats, phase-lock, LTR marks) has nothing in the 1.69–2.29 s band, and the period
even differs by *which* of our processes holds REALTIME. The periodicity is emergent from holding an
unreachable-priority queue against the WDDM scheduler on this AMD family. There is therefore no
punktfunk cadence to fix; the fix is to stop holding REALTIME, which is also canonical parity — no
shipping IDD raises it, and HIGH is the class that delivered the original encode win.

- **Driver:** the old `PFVD_NO_RT_GPU` opt-**out** (default ON) becomes the **`PFVD_RT_GPU` ladder,
  default OFF on every vendor**. Unset = no raise = canonical IDD behaviour.
- **Host:** the `pf-frame` auto-gate no longer upgrades to REALTIME. `PUNKTFUNK_GPU_PRIORITY_CLASS`
  still pins a class explicitly.

### The reanchor gate learns gap WIDTH — two new C ABI exports (ABI 19)

🛑 **Every unrecoverable loss armed the client's freeze gate twice**, and on AMD hosts the second arm
re-froze a stream that had already healed. The two signals are the frame-index gap (instant, and what
fires the RFI) and the reassembler ageing the lost frame into `frames_dropped` (~120 ms later, which
re-armed unconditionally). An LTR-RFI recovery anchor lands in ~60 ms — *between* them — so the stale
climb re-froze a bit-exact-healed picture, the host swallowed the re-ask as an RFI echo, and the
stream stayed frozen until the overdue backstop extracted a full IDR. This is the field
"H.265 freezes on every loss, AV1 fine" signature: AMF is the only LTR-RFI backend, and the slower
IDR path usually lands after the climb and dodged the race.

The gap-arm now **pre-credits** the climb it knows is coming (`ReanchorGate::arm_expecting_drops`;
the credit expires after `DROP_CREDIT_WINDOW` so a straggler-filled gap cannot mask a later real
loss), and `poll()` consumes credited climbs instead of re-arming. Plumbed through every embedder:
`pf-client-core`'s session pump, Android's sync and async loops (`note_frame_index` now returns the
gap width), and the Swift client via the two new exports —
**`punktfunk_connection_note_frame_index_ex`** and **`punktfunk_reanchor_gate_arm_expecting_drops`**.
Both originals keep their signatures and their behaviour, so an embedder that adopts neither is
unchanged; it simply keeps the race. Nothing new goes on the wire.

### ⚠ `punktfunk_send_input` now rejects an unrecognized event kind

`punktfunk_send_input` and `punktfunk_connection_send_input` **validate `ev->kind` before forming a
reference** and return `InvalidArg` for a value that is not a recognized `InputKind`. Previously the
byte was transmuted into an enum, which is UB for an out-of-range discriminant — a caller passing an
uninitialized or garbage `kind` had undefined behaviour rather than an error return. The safety
contract in the header relaxes correspondingly: `ev` need only point to *a readable
`InputEvent`-sized allocation*, not to an already-valid `InputEvent`. **If you build an event by
zeroing a struct and setting fields, nothing changes.** If you relied on an unknown kind being
silently forwarded, it is now an error.

### Linux hosts stream pad audio — the per-pad PipeWire sink (WP3)

The 0xD1 per-gamepad audio plane (DualSense haptics + speaker) was **Windows-host-only**:
`host_cap()` answered false everywhere else and `spawn()` was a stub, so a tier-A Android client
against a Linux host negotiated the capability off and fell back to wire rumble. The downstream
machinery — framer, silence gate, lanes, 0xD1 send — was already capture-agnostic; only the capturer
was WASAPI.

Linux hosts now mint **one PipeWire Audio/Sink node per DualSense-family pad**, carrying the identity
the game-side matchers read (ALSA-style `node.name` with the pad's pairing MAC, description
"Wireless Controller", bus/vendor/product/form-factor proplist, per-pad serial), 4-channel F32
48 kHz FL/FR/RL/RR, claiming no default sink, `priority.session 50`. The `process()` callback *is*
the capture. `host_cap()` on Linux = client asked **and** `PUNKTFUNK_PAD_AUDIO` **and** a reachable
PipeWire socket; the sink is minted lazily in the streamer thread. `PUNKTFUNK_PAD_SINK_NAME` /
`_DESC` override the strings for field debugging (`{pad}`/`{mac}` expand).
`PUNKTFUNK_PAD_AUDIO{,_SLOTS}` are no longer documented as Windows-only. Verified on a Bazzite 44
host: identity served through `pipewire-pulse`, rear-pair voice-coil tone captured bit-exact over
both the native and Pulse legs. The Linux sink speaks GE-Proton's AUX0–3 channel shape.

### Wake-on-LAN now works over Wi-Fi (WoWLAN)

The host's arming check asked **`ethtool`** about every NIC, which is the wrong question for
wireless: the magic-packet trigger lives in nl80211's WoWLAN state, and most Wi-Fi drivers print
`Wake-on: d` whether or not it is armed. An armed Wi-Fi host was therefore reported as *not* armed
and handed an `ethtool -s wlan0 wol g` its driver rejects. A NIC with an nl80211 phy
(`/sys/class/net/<i>/phy80211`) is now asked `iw phy <phy> wowlan show`, and the warning carries
WoWLAN-correct guidance (`iw … wowlan enable magic-packet`, plus the NetworkManager
`802-11-wireless.wake-on-wlan magic` that survives a reconnect). Two fallbacks for when `iw` cannot
answer: a **positive** ethtool reading counts (brcmfmac and friends do report there), a negative one
never does, and sysfs `device/power/wakeup` reading `disabled` is conclusive in the negative.

The **client sender** now emits from a socket bound to each non-loopback interface's own address
instead of leaving the choice to the routing table. A station in WoWLAN sleep stays associated and
its AP buffers broadcast frames until the next DTIM beacon — but only if the datagram reaches the
wireless segment at all, and with a VPN or mesh interface holding the default route
`255.255.255.255` never did. A failed bind falls back to the routed socket, so no segment is lost.

### Zero-copy capture withholds buffers until the encoder has finished reading

🛑 **Gamescope streams could tear pink at 120 fps.** The raw-dmabuf passthrough handed the SPA buffer
back to gamescope at `.process` return while the encode thread had not yet imported — let alone read
— its dmabuf, and nothing ordered the producer's writes against the consumer's read (there is no
explicit sync, and the implicit-fence wait measures `NoFence` on every compositor × vendor pairing we
have). On the direct-VCN arms (native NV12, RGB-direct EFC) the captured buffer *is* the encode
source for the whole 2-deep encode ring plus the phase-lock hold, so at 120 fps gamescope cycles back
into the buffer mid-encode: luma/chroma desync (the magenta tint) plus block corruption propagating
through the P-chain until the next intra. KDE sessions were clean because `cursor_blend` routes them
to the compute-CSC copy arm, whose read window is microseconds.

A published passthrough frame now carries a **`FrameHold`**, and the buffer rejoins the producer's
pool only when the last clone drops. The Vulkan encoder clones the hold into the ring slot at submit
and releases it when that slot's fence retires, extending "the producer must not rewrite this" across
exactly the GPU read. The host loop's repeat path is fixed by the same mechanism.

### Bazzite Game Mode no longer mirrors the box's screen (⚠ shipped-template default)

🛑 **Our own template caused it.** `packaging/bazzite/host.env` set
`PUNKTFUNK_GAMESCOPE_ATTACH=1`, and every install path — rpm, deb, Arch, nix — ships that file as
`/usr/share/punktfunk/host.env.bazzite` with the docs telling people to copy it verbatim. So the
*recommended* Bazzite setup turned the attach override on for everyone.

That override is **rung 2** of `pick_gamescope_mode`, above `dedicated_launch` at rung 3. The rung
comment calls the operator overrides a debug/CI escape hatch — correct, but we were shipping one as
a distro default, so on a Bazzite box the managed takeover and the dedicated game session were both
unreachable, and a game launched from a client's library could not get a session of its own. With a
physical display connected, attach then takes the `physical_display_connected()` arm and streams the
box's own head at the box's own mode: the mirror the field report described.

The template now forces nothing and lets per-connect detection answer, which on a box with
`gamescope-session-plus` is MANAGED. Attach stays available, documented as the opt-in it is, with the
mirror and the dedicated-session cost stated.

### `edid_lock` — pin AMD connector EDID emulation while streaming (EXPERIMENTAL)

A new display-policy axis beside `ddc_power_off` / `pnp_disable_monitors`, orthogonal to presets and
**off by default**. At the first Exclusive isolate the host pins each occupied AMD connector's live
EDID plus `ADL_EMUL_MODE_ALWAYS` — the software equivalent of an HPD-holding dummy plug — **before**
the physicals deactivate, so an awake sink answers its own live-EDID read; last-member teardown
unlocks. It targets the standby-sink stall class at its source: with emulation pinned the kernel-mode
driver stops servicing the sleeping sink's HPD/DDC/link.

Pinned emulation outlives the process, so a crash journal (`edid-lock-active.json`) unlocks on the
next host start, mirroring the `pnp_disable_monitors` recovery. Inert without an AMD driver
(`atiadlxx.dll` absent) and on non-Windows. The ADL FFI lives once in `pf_win_display::adl_emul`, so
the new **`display-disturb adl-emul`** probe and the host exercise byte-identical driver calls. The
console shows the toggle **only** when the GPU inventory lists an AMD adapter — a toggle that can
never act is exactly the "saved, then did nothing" trap the enforced-axes list exists to prevent.

### An over-declared stream level no longer demotes native Vulkan decode

A HEVC stream whose declared level exceeds what the device advertises is now treated as a **clamp**
rather than a refusal, so native Vulkan decode survives an encoder that over-declares. The Windows
client legs also build again: the edition-2024 `clients/session` binary could not compile on Windows,
and `pf-presenter` now spells `MAKEINTRESOURCE(1)` as `ptr::without_provenance` — clippy 1.96's
`manual_dangling_ptr` reads the integer-ordinal cast as a dangling pointer and fails the Windows
`-D warnings` gate, which was masked on main by the client bins failing to build first.

### Library, launcher marks and plugin-kit 0.4.1

- **Launcher tiles carry their launcher's mark.** A brand **token** goes on the wire (`steam`,
  `heroic` — never bytes, never a URL) and each client draws the vector it already ships. `icon`
  joins `GameEntry` and `CustomEntry` in the management API, and is hand-settable for the same
  reason `role` is: an operator's own "Steam" tile should be able to look like one.
- **`@punktfunk/plugin-kit` 0.4.1 publishes the `icon` field.** The kit had shipped the field
  without a version bump, so no plugin could name its mark.
- **Every pinned card gets a library, and it launches with that card's profile.**

### Decky: one library shortcut, not one per boot

🛑 A boot race minted a **new** Steam library shortcut on every plugin load, so the library
accumulated duplicates indefinitely. Fixed, and the plugin no longer toasts on every launch and every
failed panel refresh.

### A stats tier picked between streams now applies without a restart

The console latched the stats tier at stream start, so a tier chosen between two streams reached
nothing until the app was restarted.

### Miri, sanitizers, and the lint ratchets

- **Miri** now interprets the FFI-free leaf crates, one of them at **MSVC layout**. It immediately
  earned its place: `pf-driver-proto`'s legacy-`AddRequest` test read a `[u8; 40]` (align 1) through
  `bytemuck::from_bytes`, which takes a *reference into* the buffer and panics unless that buffer
  happens to be 8-aligned — as a stack array usually is. Now `pod_read_unaligned`.
- **ASAN + LSAN over the C ABI boundary**: a `c-abi-asan` job in `audit.yml` runs the harness under
  both, weekly and on demand, behind a `PF_SAN` sanitizer gate.
- **Two soundness fixes**: `InputKind` is validated before a `&InputEvent` is formed (above), and the
  Windows `TOKEN_USER` buffer is properly aligned with `EqualSid` made to fail closed.
- **WP4**: `AvFrame`/`AvSwsContext` are RAII across all three libav backends in `pf-encode`.
- **The lint ratchets (WP2b + WP2c)**: crate-level gaps closed, the unsafe lints hoisted into the
  workspace tables across all three workspaces, and three blocking unsafe-hygiene grep gates in
  `ci.yml`. The two bindings-only `sys` crates are explicitly exempted from the hoisted deny.

### Dependencies, audit and licences

The 2026-08-13 dependency sweep, acted on in full:

- **Security:** `event-listener` 5.4.1 → 5.4.2 (RUSTSEC-2026-0221, unsound `Send`/`Sync`);
  `spin` 0.9.8 → 0.9.9 (0.9.8 is **yanked** and was genuinely compiled); `wayland-scanner`
  0.31.10 → 0.31.11, which moves `quick-xml` 0.39 → 0.41 and lets **both** RUSTSEC-2026-0194/0195
  ignores be deleted rather than left as permanent exceptions. Only RUSTSEC-2023-0071 (`rsa` Marvin,
  still unfixed upstream) remains.
- ⚠ **Two CI gates that scanned nothing.** `cargo audit` only ever reads the **root** `Cargo.lock`,
  so the drivers lock was in the job's `paths:` filter while being ignored; all four secondary
  workspaces now get an explicit `--file`. And `packaging/windows/pf-vkhdr-layer` had **no lockfile
  at all** while shipping as a DLL in the host installer, so neither cargo-audit nor cargo-about had
  ever seen it — lockfile generated, committed, and added to `paths:`. `audit.toml` now also says out
  loud that `cargo audit` reports unsoundness as a *warning* and the job fails only on
  vulnerabilities, which is why the `event-listener` advisory sat unnoticed.
- **13 unused dependencies removed from `punktfunk-host`** (the Wayland stack, xkbcommon, reis,
  khronos-egl, ash, usbip-sim, parking_lot, bytemuck) — the code moved to `pf-inject`/`pf-zerocopy`
  in the subsystem extraction and those crates declare them; only the manifest entries and their
  now-false comments stayed. Plus unused `bytes`, `anyhow`, `tracing`, `serde` in five other crates,
  and the high-level `wdk` crate from all five driver crates.
- **Latent breakage fixed** — crates that compiled only through feature unification now declare what
  they use: `pf-inject` (`tokio` `macros`), `pf-capture` (`tokio` `sync`), `pf-client-core` (two
  windows-rs headers). `pf-console-ui` took `pf-client-core` **without** `default-features = false`,
  unlike every other consumer; that default compiles the vendored PyroWave C++, which is fatal on
  Windows ARM64 and only safe today because that leg passes `--no-default-features`.
- **Licences:** `ring`'s `OpenSSL` exception and its per-crate acceptance are retired now that ring
  is gone. THIRD-PARTY-NOTICES regenerated — 601 → 580 → 582 crates across the sweep.

### The dependency currency wave — thirteen majors, and a silently-disabled AES path

The currency half the sweep above deferred, landed as one wave. Most of it is version hygiene, but
one item is a real defect and one changes a build flag you may be carrying.

🛑 **Hardware AES was silently off on every Android build.** `aes` 0.8 enabled the ARMv8 AES
instructions on aarch64 only behind `--cfg aes_armv8`, and `polyval` 0.6 gated its PMULL GHASH path
behind `--cfg polyval_armv8` — both set in `.cargo/config.toml`. A `RUSTFLAGS` environment variable
**overrides config rustflags entirely**, and `cargo-ndk` sets its own for every Android build, so
those two cfgs vanished and the per-packet decrypt path fell back to **software AES**. `aes` 0.9
runtime-detects through `cpufeatures` and `polyval` 0.7 selects its armv8 backend by `target_arch`,
so neither cfg exists any more and the flags are **deleted** from `.cargo/config.toml`. If you carry
a fork of that file, drop them: they are dead, and keeping them costs nothing but confusion.

- **The RustCrypto family moves as ONE change** — `aes` 0.9, `aes-gcm` 0.11, `sha2` 0.11, `hmac` 0.13,
  `cbc` 0.2, `chacha20poly1305` 0.11. They share the `crypto-common`/`digest` traits, so a partial
  bump strands crates on trait generations that cannot interoperate. The API generation forces
  `AeadInPlace` → `AeadInOut` (`{encrypt,decrypt}_inout_detached` over `InOutBuf`), `generic-array` →
  `hybrid-array`, `Mac::new_from_slice` → `KeyInit::new_from_slice`, and the `BlockCipher*`/
  `BlockMode*` renames. ⚠ **The GameStream wire formats are untouched** — AES-128-ECB no-padding, the
  CBC audio path and the GCM control-stream seal all keep their exact byte behaviour; only type
  plumbing moved.
- ⚠ **`rsa` 0.9 cannot come along**: it is built on `digest` 0.10, whose 0.11 line is release-candidate
  only — not something the Moonlight pairing ceremony should ride. The three sites where a digest is
  an `rsa` *type parameter* now name `rsa::sha2::Sha256` explicitly; everything else is on sha2 0.11.
- **`skia-safe` 0.87 → 0.99** in `pf-console-ui` — twelve releases carrying Skia milestones 140–150.
  Only three reach us: m143 **deleted `SkPath`'s mutating API** (geometry is built through
  `PathBuilder` and frozen with `snapshot()`/`detach()`; 34 errors over eight call sites), 0.93
  deprecated `gradient_shader` for `gradient` (a warning, but the gate runs `-D warnings`), and the
  Vulkan surface path came through untouched.
- **`wasapi` 0.23 → 0.24.** ⭐ 0.24 fixes upstream the dangling-`PCWSTR` bug this tree routes around
  in five places — `DeviceEnumerator::get_device` built its argument as
  `PCWSTR::from_raw(HSTRING::from(id).as_ptr())`, dropping the `HSTRING` at the end of that statement
  so `GetDevice` read freed memory. The five comments asserting that bug in the present tense are
  corrected. ⚠ **The workarounds stay** — `open_wasapi_device` is still the one resolution path whose
  errors name the endpoint id, and `device_by_id` additionally filters to ACTIVE endpoints, which the
  crate's `get_device` does not. Removing them would be a behaviour change, not currency.
- **Ten more**: `jni` 0.21 → 0.22 (the Android bridge), `rcgen` 0.13 → 0.14, `rand` 0.8 → 0.9 (the
  host was the last crate on the old major), `base64` 0.22 → 0.23, `x509-parser` 0.16 → 0.18 — which
  takes `thiserror` 1.0 out of the host graph entirely — `libloading` 0.8 → 0.9 across the five crates
  that `dlopen`, `mdns-sd` 0.20 → 0.21 with `if-addrs` 0.13 → 0.15 (together, they share types),
  `x11rb` 0.13 → 0.14, `xkbcommon` 0.8 → 0.9, `reis` 0.6.1 → 0.7.1, `windows-service` 0.7 → 0.8
  (removing the last `windows-sys` 0.52 in the tree), `android_logger` 0.14 → 0.15, and `criterion`
  0.5 → 0.8 (dev-only, benches).
- **New test coverage**: the TLS 1.2 Moonlight handshake, and the post-quantum group is pinned by a
  test so a backend change cannot silently drop it.
- THIRD-PARTY-NOTICES regenerated across every client and the host for the wave.

### Documentation and the docs site

⚠ **`docs-site/public/openapi.json` had drifted far behind `api/openapi.json`** — it was stamped
`0.21.0` against the checked-in spec's `0.27.0`, and was missing five endpoints (`/library/hidden/{id}`, `/plugins/logs`, and all
three `/update/*` routes), so the published API reference described a host nobody was running. The
copy is a documented manual step (`cp api/openapi.json docs-site/public/openapi.json`) that nothing
in CI enforces, and it had simply been skipped. Re-synced for this release; the two files are now
byte-identical.

⚠ **It drifted again within the same release cycle** — the scanner-removal regen updated
`api/openapi.json` and not the docs-site copy, which is the failure mode repeating in miniature.
Re-synced a second time. **Until something gates it, treat `cp api/openapi.json
docs-site/public/openapi.json` as part of regenerating the spec, not a follow-up.**

### CI

- The C/C++ half of the build is cached and links with **mold**; the debug/release target caches no
  longer collide.
- `release.yml` folds into `apple.yml`, and the two Windows-client workflows consolidate into one.
- The web console builds **once per push** instead of once per packaging job.
- The `smoke-install` job (see the Debian section) installs every published package from the registry
  in pristine `ubuntu:24.04`, `ubuntu:26.04` and `debian:trixie` images and asserts the served
  version is the one the run just built.
- ⚠ Gate C counted **comments**: a comment that named the env mutators verbatim satisfied the gate it
  was documenting.

## v0.27.0

87 commits since v0.26.0.

### Versions

| | v0.26.0 | v0.27.0 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged |
| C ABI | 17 | **18** | `punktfunk_connection_next_rumble_cmd2` **added**; nothing removed or changed |
| Workspace crate dirs | 26 | **27** | `crates/punktfunk-encode-worker` (39 members; two `tools/` crates deliberately *excluded*) |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3) |
| Windows virtual-gamepad channel | 3 | **3** | unchanged — three `device_type`s added additively |
| Plugin index schema | 1 | **1** | unchanged |
| `api/openapi.json` | 0.25.0 | **0.25.0** | unchanged — no management-API edits this release |
| gamescope patch level (`+pfhdrN`) | 4 | **5** | 6 patches → 7 (the PipeWire use-after-free); `pkgrel` resets 3 → 1 |
| `@punktfunk/host` (SDK) | 0.1.4 | **0.1.4** | unchanged |
| `@punktfunk/plugin-kit` | 0.4.0 | **0.4.0** | unchanged |

⚠ **`crates/pf-driver-proto` is no longer byte-identical to the previous release.** It was through
both v0.25.0 and v0.26.0, so if you ship the virtual-display driver or the gamepad channel and have
been skipping this crate, stop skipping it here. The change is purely additive — three `device_type`
constants, no field moved, no size changed.

### ⚠ Breaking changes

**None** for embedders or the wire. Every embedder, packager and plugin that works against v0.26.0
works against v0.27.0 unchanged; the C ABI moves, but by addition only (below).

Two things change shape for **packagers** and one **default** flips:

- **A second installed binary**, `punktfunk-encode-worker` — see the section below. It is the only
  file that may carry `cap_sys_nice=ep`, and it must be a separate file.
- **`PUNKTFUNK_XBOX_BACKEND` now defaults to `hid`** on Windows, so an Xbox pad is built as a real
  HID device rather than the XUSB companion. `=xusb` is the escape hatch.
- **NixOS `scripting.autoStart` now defaults ON**, matching every other packaging (detailed below).

### `punktfunk-encode-worker` — the GPU-priority capability moves off the host

0.26.0 left the PyroWave priority ladder wired and inert: it needs `CAP_SYS_NICE`, and 0.26.0-1
proved the host can never hold one — see **PyroWave on Linux — Wave 2**, PW1, under v0.26.0 below. A
capability-carrying process cannot be identified by KWin (`cap_ptrace_access_check` refuses
`/proc/<pid>/exe` to a reader whose effective set is not a superset of the target's **permitted**
set), so it never gets `zkde_screencast_unstable_v1` and every KDE desktop session dies. Neither
`prctl(PR_SET_DUMPABLE, 1)` nor systemd `AmbientCapabilities=` nor a NixOS `security.wrappers` entry
changes that — all three land the capability in the same permitted set.

The capability therefore moves to a process that fronts nothing. **`punktfunk-encode-worker`** is a
new workspace member and a new installed binary: it owns the priority-elevated Vulkan device for
PyroWave sessions, receives capture dmabufs over a `SOCK_SEQPACKET` pair from its parent, and returns
compressed access units. It connects to no compositor, no D-Bus and no network, so its
non-dumpability costs nothing and its blast radius is one socket to the host that spawned it.

🛑 **The invariant, for anyone packaging this:** the worker is a **separate file**. Never a hardlink
to `punktfunk-host` and never a subcommand of it — a shared inode shares the file capability, which
silently re-creates 0.26.0-1 on every KDE box. `punktfunk-host` carries no capability, on any
channel, ever.

- **The grants are re-targeted, not re-introduced.** Every channel that granted in 0.26.0-1 grants
  again, at the worker: Arch `.install` (`post_install` **and** `post_upgrade` — a replaced binary is
  a new inode), RPM `%caps(cap_sys_nice=ep)` in `%files` (never a `%post setcap`; this covers Fedora
  and Bazzite layering), the Bazzite sysext staging tree pre-`mksquashfs` (which does record
  `security.capability`), the deb `postinst`, the Deck installer, and NixOS
  `security.wrappers.punktfunk-encode-worker`. Every #136 host-side removal stays verbatim, including
  the sysext's host hard-fail.
- **The sysext assertion is amended, not removed** — host must be empty (hard fail), worker must
  carry **exactly** `cap_sys_nice=ep`. A *missing* worker capability is not an error: the grant is
  best-effort everywhere.
- **A new release-CI leg asserts the getcap matrix** on the built Arch package, the deb and the
  mounted sysext raw. The 0.26.0-1 lesson was "verify the package, never the board"; this is that,
  mechanized, and it is what would have caught the original break.
- **On NixOS the env override is load-bearing**, not a convenience: a file capability cannot live on
  a read-only store path, so the module wraps the worker and sets `PUNKTFUNK_ENCODE_WORKER` to the
  wrapper path in the unit. An ambient grant is fine *here* — the worker is not a KWin client. The
  host's `ExecStart` stays on the plain store path (the #136 fix stands).

**Fallback ladder — no rung can kill a negotiated session.** Binary not found → spawn failure →
handshake timeout → protocol or workspace-version mismatch → socket EOF mid-session all fall back to
the **in-process encoder exactly as today**, at default priority, with one warning. Host and worker
are different files now, so the version check is load-bearing rather than decorative; they ship
lockstep in every channel. The in-process path stays compiled and tested — it is the floor, not dead
code. `PYROWAVE_QUEUE_PRIORITY` keeps its 0.26.0 grammar and is now forwarded **explicitly** in the
handshake rather than read from the worker's environment, which is sanitized at spawn; one env var
still means one thing on both platforms.

### NixOS — session detection, module defaults, and a CI gate that was never running

🛑 **The host could not detect any graphical session on NixOS, at all.** The live-session probe
matched `/proc/<pid>/comm` exactly against `kwin_wayland` / `gamescope` / `gnome-shell` /
`Hyprland`. `comm` is the kernel's name for the **executed file**, truncated to 15 bytes — not
`argv[0]` — and nixpkgs wraps essentially every graphical binary: `wrapProgram` moves the real ELF
aside to `.<name>-wrapped` and installs a wrapper that `exec -a "$0"`s it. So the kernel reports
`.kwin_wayland-w` while `ps` and `pgrep -a` show a perfectly ordinary `kwin_wayland`, because they
read argv. Every probe answered `ActiveKind::None` on a running desktop, and nothing downstream
could recover: `wayland` logged as `-`, a correct `WAYLAND_DISPLAY` changed nothing, `Auto` returned
the *detected* backend so a live KWin already in `available()` was never chosen, and a
`PUNKTFUNK_COMPOSITOR` pin turned the miss into a hard error through `pinned_at_a_dead_session`.
sway and river survived by accident — nixpkgs' wrapper execs a binary still called `sway`.

Names are now resolved through `/proc/<pid>/exe`, whose file name is untruncated, with the nixpkgs
decoration stripped. Stripping requires **both** the leading `.` and a trailing `-wrapped`, so
KWin's own real `kwin_wayland_wrapper` binary keeps its name instead of collapsing into
`kwin_wayland` and handing the probe the parent's PID. The `comm` fast path is unchanged for every
ordinary distro — one read, no readlink — and no name that matched before can stop matching. Also
applied to the foreign-gamescope probe, which had the same defect.

**Module changes** (`services.punktfunk`):

- **`host.desktopSession`** *(new, default `false`)* — binds the host to `graphical-session.target`,
  the declarative form of the `punktfunk-host-desktop-session.conf` drop-in. Without it a
  Plasma/GNOME restart leaves the host holding a Wayland socket and portal D-Bus connection that
  died with the old compositor: it still listens, still answers, and every session after that fails
  at capture. Off by default because an appliance may never reach that target and would be left
  permanently stopped.
- ⚠ **`scripting.autoStart` now defaults ON** *(behaviour change)*, matching the deb `postinst` and
  RPM `%post`, which both `systemctl --global enable` the runner, and the sysext's baked-in
  `default.target.wants` symlink. It was opt-in here on the reasoning that the runner is inert until
  you add automation — untrue since the game-library scanners became plugins, so a NixOS host came
  up with an empty library and no obvious cause. Opt out with `scripting.autoStart = false` or
  `systemctl --user mask punktfunk-scripting`.
- **Three divergences from the shipped units, ported.** `punktfunk-web` gains
  `StartLimitIntervalSec=0` (without it, 5 starts / 10 s against `RestartSec=2` gives up permanently
  after ~10 s — exactly the window before the host's first `serve` writes the mgmt token, so a
  console enabled before the host's first run stayed dead) and `Restart=always` rather than
  `on-failure`. `punktfunk-scripting` gains the sandbox the deb/rpm unit has all along
  (`NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths=%h /tmp`, restricted address families,
  `PrivateTmp=no`) — it is the one unit that runs arbitrary operator TypeScript by design, and it
  had been running strictly less confined on NixOS than anywhere else.
- A **warning** when the host is enabled and `xdg.portal.enable` is not.

🛑 **`nix flake check` does not check `nixosModules`** — worth knowing for anyone maintaining a
flake. It forces the value and asserts it is a lambda taking an open attribute set, and stops;
nix's source still carries `// FIXME: if we have a 'nixpkgs' input, use it to check the module.`
Measured: a module with a nonexistent option, a nonexistent `pkgs` attribute **and** a nonexistent
`lib` function passes, printing `checking NixOS module ... all checks passed!`. `nix.yml`'s header
claimed that leg covered the module; it never had. `checks.<system>.nixos-module`
(`packaging/nix/module-check.nix`) now evaluates it against real nixpkgs across four scenarios and
asserts on the rendered units, including a guard that the host's `ExecStart` stays on the plain
store path while the encode worker points at the wrapper. Its assertions are pure Nix, so
instantiation runs them and the existing `--no-build` leg is enough. `punktfunk-gamescope` gains a
`build-gamescope` dispatch input — it is on the critical path of every host build yet nothing
compiled it, and it tracks nixpkgs' gamescope, so a `flake.lock` bump is what breaks it.

### C ABI 17 → 18

**`punktfunk_connection_next_rumble_cmd2` is new.** The `0xCA` rumble plane carries the two Xbox
impulse-trigger motors (v3, below) and `punktfunk_connection_next_rumble_cmd`'s fixed out-params
have no room for them:

```c
PunktfunkStatus punktfunk_connection_next_rumble_cmd2(
    PunktfunkConnection *c, uint16_t *pad, uint16_t *low, uint16_t *high,
    uint16_t *left_trigger, uint16_t *right_trigger,
    uint32_t *backstop_ms, uint32_t timeout_ms);
```

**Added, not widened.** `_cmd` keeps its signature *and* its values bit-identical for handle-only
traffic; all four rumble entry points remain exported. An exported parameter list is part of the
contract, and growing one in place breaks every out-of-tree embedder at once — with a
stack-corruption signature rather than a link error. This follows the existing
`next_rumble` → `next_rumble2` precedent.

⚠ **One behavioural delta on the old symbol**, documented in `abi.rs` and pinned by a test: against
a host driving the trigger motors, a `_cmd` caller now receives commands with `low == high == 0`
where the demux previously dropped the update entirely. They are idempotent handle stops — the
command as a whole is not silent, so redundant-stop suppression cannot fold them. Zero cost today:
nothing sources non-zero trigger levels yet.

**Render trigger levels only on a pad that has trigger motors.** Do not fold them into the handles —
impulse-trigger content is continuous, so folding it drones the handle motors flat-out. Query
`SDL_PROP_GAMEPAD_CAP_TRIGGER_RUMBLE_BOOLEAN` or `GCDeviceHaptics.supportedLocalities`.

🛑 **This delivery path is deliberately built ahead of its producer and nothing here claims
otherwise.** Exactly one backend can ever source these levels — the Windows HID Xbox pad's output
report `0x03` — because `XINPUT_VIBRATION` and evdev `FF_RUMBLE` both have two members. That
producer is reachable only through GameInput, which does not enumerate an `xinputhid`-promoted Xbox
pad at all (measured against a real Microsoft Elite, equally invisible there while classic XInput
reads it live). The wire, the engine and this entry point are exercised by synthetic levels only.

### Gamepads

- **`PUNKTFUNK_GAMEPAD_XBOXELITE = 11`** — a new `GamepadPref` wire byte, appended to
  `Hello`/`Welcome`. The `Auto` sentinel in the round-trip test moved 11 → 12. An older peer
  degrades an unknown byte to `Auto`, so this is graceful in both directions.
- **`XboxOne` is now a distinct HID identity on Windows** (`045E:02FD`, Bluetooth Xbox One S)
  through the UMDF minidriver. It used to fold to `Xbox360` there, because the only Windows Xbox
  backend was the XUSB companion, which presents one fixed 360 identity and cannot vary it.
- **Three new `pf_driver_proto::gamepad` device types**, contiguous and sharing one report
  descriptor byte for byte (they are the same pad in HID terms; the descriptor is the report
  *shape*, the identity is what the OS keys mappings off):

  | const | value | identity |
  |---|---|---|
  | `DEVTYPE_XBOX` | 4 | `045E:0B13` Xbox Wireless Controller |
  | `DEVTYPE_XBOX_ONE_S` | 5 | `045E:02FD` Xbox Wireless Controller (One S) |
  | `DEVTYPE_XBOX_ELITE` | 6 | `045E:0B22` Xbox Elite Wireless Controller Series 2 |

  ⚠ The Xbox input report is **not** 64 bytes like its siblings — it is `XBOX_INPUT_REPORT_LEN`
  (16). The driver serves per-identity report lengths, because hidclass sizes its buffer from the
  descriptor and refuses an over-long source.
- ⚠ **Elite paddles are not implemented.** `BTN_PADDLE1..4` still fold or drop exactly as on the
  other Xbox classes. `DualSenseEdge` remains the only virtual pad with native back-button slots.
- **All three Xbox identities install `pfGamepadXbox`**, their own DDInstall section, which attaches
  the `xinputhid` bus filter. Merging it back into the shared `pfGamepad` section is a one-line edit
  that looks like tidying and would hand a DualSense, DualShock 4, Edge and Steam Deck to
  Microsoft's Xbox translator. `only_the_xbox_identity_installs_the_xinputhid_section` asserts the
  split in both directions.

**What actually promotes the pad — two registry values, and the pairing is the whole finding.**
`UpperFilters=xinputhid` is a `.HW` AddReg (hardware key); `DevicePropertyFlags=1` is a DDInstall
AddReg (software key). A one-value A/B on real hardware: removing `DevicePropertyFlags` alone
reverts everything — no `IG_00`, no XUSB interface, no XInput, no WGI entry — while `UpperFilters`
alone is completely inert. `1` = `BusDevice`, which Microsoft's own comment glosses as "a focused
bus filter driver for the IG_ problem". **This retracts an earlier in-tree conclusion that the
filter should never ship**: it was never broken, it had simply never been switched on.
⚠ Microsoft's allow-list contains `02D1, 02DD, 02E3, 02EA, 0B00, 0B0A, 0B13, 02FF` — neither `02FD`
nor `0B22` is on it, and promotion happens anyway, because it comes from our own AddReg.

### Wire (no version change)

**The `0xCA` rumble datagram gains a v3 form**, `PUNKTFUNK_RUMBLE_V3_LEN = 14`:

```
v1   7 B: [0xCA][u16 pad][u16 low][u16 high]
v2  10 B: … [u8 seq][u16 ttl_ms]
v3  14 B: … [u16 left_trigger][u16 right_trigger]
```

v3 is built *from* v2's bytes, so the prefix relationship is structural rather than a convention two
encoders must keep agreeing on, and every reader gates with `>=`. All four levels share one `seq`
and one TTL deliberately: they are one statement of the pad's feedback at one instant, so the entire
v2 apparatus — renewal cadence, stop burst, the client's seq gate, the lease clamp — governs the
triggers with no new code. The new `RumbleUpdate` fields are plain `u16`, not `Option`: on a
level-triggered plane "absent" must mean zero, because "absent → keep the previous value" is the
stuck-rumble bug in a new costume.

⚠ **The two trigger `enable`-mask bits remain conjecture.** Bits 2/3 (the handles) are measured;
bits 0/1 are inferred from field order and nothing else. No test asserts them. XInput cannot settle
this; it has two motors.

### Packaging

- **gamescope pin `8c676c39` → `5fb8dce4`** (3.16.25-1 → 3.16.25-11), all six patches rebased, plus
  a **seventh**: the PipeWire use-after-free that aborted a session on every connect. The marker
  moves `+pfhdr4` → **`+pfhdr5`**, so `pkgrel` resets to 1.
- **Patch 0001 offers `xBGR_210LE` before `xRGB_210LE`.** ⚠ Deliberately *not* done by calling
  upstream's `vulkan_get_rgb10_capture_format()` — that symbol landed after 3.16.25 and would break
  `packaging/nix/gamescope.nix` with an opaque C++ error instead of a patch conflict.
- **Every `punktfunk-gamescope` RPM ever published was unsigned.** `Sign RPMs` runs right after
  `Build RPM`, while the gamescope RPM is built ~90 steps later behind its own cache, so it missed
  the signing pass entirely — and the repo file we ship carries `gpgcheck=1`. A second pass signs it
  before publish, fail-closed on a tag.
- ⚠ **The v0.26.0 gamescope gate failed the job at the *build* step**, which in `deb.yml` runs before
  both the apt publish and the release attach — so a missing *extra* withheld the host `.deb` itself,
  and the `.deb` published on v0.26.0 still carries the `CAP_SYS_NICE` grant. `rpm.yml` had the
  identical latent bug. Both now warn at build/package time and gate as the **last** step of the job.
- **`driver uninstall --audio`** — a third Inno `[UninstallRun]` entry that removes the MEDIA-class
  devnodes the host mints at runtime. Marker-matched, never name-matched: our instances are
  name-identical to Steam's, and a `ROOT\` enumeration guard means a marker-shaped value on a real
  sound card can never cost the user their hardware.
- **The sysext `post_merge` step re-runs when already current, plus a new `reapply` verb.** A sysext
  upgrade is driven by the script from the **old** image, so a `post_merge` step added in a release
  is executed by nobody, permanently, on exactly the installs that need it.

### Host

- **HDR capture offers `xBGR_210LE` before `xRGB_210LE`.** gamescope's capture textures are
  mappable, hence linear-tiled, and NVIDIA does not implement linear-tiled STORAGE for
  `A2R10G10B10_UNORM_PACK32` — so `imageStore` lands in XBGR order while the buffer is still
  *labelled* `XRGB2101010`. Every mapping on both ends audits clean because the label was right and
  only the content was wrong. Fixed host-side because the deployed gamescope cannot self-correct.
- **One NVENC open failure no longer kills every session on the box**, and the 10-bit capability
  probe no longer wedges a direct-SDK host process-wide with `NV_ENC_ERR_INVALID_VERSION`.
- **`/api/v1/local/summary` reports the resolution the session actually got**, not the negotiated
  one it was seeded with.

### Workspace

`crates/punktfunk-encode-worker` joins as a member (above). Two bring-your-own-hardware measurement
tools are added and **excluded** in the root manifest, so `cargo build --workspace` and CI never see
them: `tools/hid-descriptor-dump` (dumps and decodes a real HID report descriptor; pulls `hidapi`)
and `tools/win-input-matrix` (asks each Windows input API what it can see — ⚠ `wake_wgi()` is not
optional there: both WGI collections return a cache a console app has never started filling, so
without subscribing first they come back empty with real controllers attached).

### Host and client environment variables

- **`PUNKTFUNK_XBOX_BACKEND`** *(new, host, Windows)* — `hid` (the new **default**) or `xusb` (the
  escape hatch). The HID pad is now a superset of the XUSB companion: it keeps classic XInput while
  gaining Steam, SDL, RawInput, DirectInput, `joy.cpl` and WGI, plus rumble, which XUSB could not
  source at all. The escape hatch stays because promotion leans on Microsoft's inbox
  `xinputhid.inf`; if a servicing update changes it, one env var restores the old behaviour with no
  reinstall. An unrecognised value takes the **default**, not the opt-out, so a typo cannot silently
  drop a user onto the path with no HID collection.
- **`PUNKTFUNK_GAMESCOPE_BIND`** *(new, host, Linux)* — unset = auto, `0` = never, `1` = force.
  Governs whether the host binds the patched gamescope over the distribution's `/usr/bin/gamescope`
  inside a session's mount namespace.
- **`PUNKTFUNK_ENCODE_WORKER`** *(new, host, Linux)* — where to find the encode worker. Resolution
  order: this variable → alongside `/proc/self/exe` → `PATH`. `off` forces the in-process encoder,
  the debug escape hatch that makes the A/B a one-line change. Load-bearing on NixOS (above).
- **`PYROWAVE_QUEUE_PRIORITY`** *(unchanged grammar, new consumer)* — the *intent*, forwarded to the
  worker; the granted class comes back in the handshake and the host logs it centrally, so the
  in-process INERT warning does not double-fire. When the worker is uncapped as well — an operator
  stripped it, or the filesystem cannot store the capability — the same INERT wording fires, now
  naming the worker binary rather than the host.

### Documentation

- `docs-site` **Running as a service → GPU scheduling priority** rewritten around the split: the
  worker carries the capability, the host never does, and `setcap` on `punktfunk-host` is called out
  as the thing an operator must never do, with the `zkde_screencast_unstable_v1` symptom spelled out
  so anyone who already did it can self-diagnose. The anchor is unchanged, so existing links hold.
- `configuration.md` gains the `PUNKTFUNK_ENCODE_WORKER` row and rewrites `PYROWAVE_QUEUE_PRIORITY`
  off "the packages deliberately do not grant this".
- The 0.26.0 user-facing notes describe a privilege that is deliberately not granted. That is the
  record of what 0.26.0 shipped and is **not** rewritten; the new phrasing — granted to the worker,
  never to the host — lives in `docs/releases/v0.27.0.md`.
- `install.md` **NixOS** documents `desktopSession`, and its `punktfunk-scripting` bullet no longer
  claims the runner "ships disabled": that was true only of Arch and source installs — apt, dnf, the
  Bazzite sysext and now the NixOS module all start it, because the library scanners are plugins.
  `bazzite.md` carried the same stale claim and is corrected. **Running as a service → Restart the
  host with your desktop** gains the NixOS one-liner beside the drop-in.
- `packaging/nix/README.md`: `desktopSession`, `gamescopeHdr`/`gamescopePackage` and the
  `punktfunk` group added to the option tables; the "what the module configures" list gains the
  `security.wrappers` entry, with the KWin-identification reasoning for why the capability is on the
  worker and not the host; and a caveat recording that `nix flake check` does not check the module,
  plus the two rules for editing `module-check.nix`.

---

## v0.26.0

52 commits since v0.25.0.

### Versions

| | v0.25.0 | v0.26.0 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged |
| C ABI | 17 | **17** | unchanged — no symbol added, removed or changed |
| Workspace crate dirs | 26 | **26** | unchanged (40 workspace members) |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3) |
| Windows virtual-gamepad channel | 3 | **3** | unchanged |
| Plugin index schema | 1 | **1** | unchanged |
| `api/openapi.json` | 0.24.0 | **0.25.0** | tracks API edits, lags one release by convention |
| gamescope patch level (`+pfhdrN`) | 2 | **4** | 3 patches → 6; `pkgrel` 1 → 2 |
| `@punktfunk/host` (SDK) | 0.1.2 | **0.1.4** | |
| `@punktfunk/plugin-kit` | 0.3.2 | **0.4.0** | the `plugin` launch kind |

`crates/pf-driver-proto` is byte-for-byte identical to v0.25.0 and to v0.24.0 — if you ship the
virtual-display driver or the gamepad channel, the last two releases have not touched you.

### ⚠ Breaking changes

**None.** This is a fixes release. Every embedder, packager and plugin that works against v0.25.0
works against v0.26.0 unchanged. Two behaviour changes are worth knowing about anyway, because both
make a client advertise *less* than it used to — see **Capability advertisement** below.

### Capability advertisement

- **`VIDEO_CAP_444` is now probed, not asserted.** It rode the "Full chroma" setting alone. That was
  safe while a software HEVC decoder sat underneath it; M8 removed one (there is no permissively
  licensed HEVC CPU decoder, so `software_decodable_codecs()` is `H264|AV1`). The host grants 4:4:4
  on HEVC **only** and answers the resolved chroma in the `Welcome` *before* the client builds a
  decoder — so on a device with no 4:4:4 decode the toggle did not cost crispness, it cost the whole
  codec: the Vulkan rung refuses the shape at construction, VAAPI refuses it too, there is no CPU
  rung, and the session reconnects on H.264. No AMD silicon has HEVC 4:4:4 decode, so every Steam
  Deck with that switch on lost HEVC. Per-profile and default-off, which is why it read as
  intermittent.

  Now gated on `hevc_444_hardware_decodable`, which asks the driver through the same code the rung
  uses at construction (`VkH265Decoder::probe_stream_support`). **Both depths are required**, not
  either: with HDR the host may resolve 4:4:4 10-bit, and a device offering `YUV444_8` but not
  `YUV444_10` lands in the same hole. Answering from the Vulkan rung alone is exact rather than
  approximate — it is the only rung in this build that implements 4:4:4 at all
  (`pf_vaadec::profile_for` errors on `chroma_format_idc 3`, pf-dxvadec refuses anything but 4:2:0,
  the CPU rung is 8-bit 4:2:0).

  ⚠ Deliberately **not** extended to `VIDEO_CAP_10BIT`/HDR: all three rungs implement 10-bit 4:2:0,
  so a Vulkan-only probe there would withdraw HDR from boxes whose VAAPI/DXVA rung decodes it
  perfectly — a regression against a case never observed.

  The bit arithmetic moved into `video::video_caps_for` so the part that was wrong is testable
  without a GPU, a host or a `Hello`; the test is verified non-vacuous against the planted defect.

### Host and client environment variables

Four new, one clarified. Verified new by `git grep` at the v0.25.0 tag, not assumed —
`PUNKTFUNK_JUMBO`, `PUNKTFUNK_WIRE_MTU`, `PUNKTFUNK_STREAMED_AU`, `PUNKTFUNK_LIBRARY_ART_ROOTS`,
`PUNKTFUNK_RECOVER_SESSION_CMD`, `PUNKTFUNK_GAMESCOPE_SDR_NITS`, `PUNKTFUNK_MAX_FPS` and
`PUNKTFUNK_ON_CONNECT_CMD` all already existed.

- **`PUNKTFUNK_OVERLAY_MASK`** *(new, client)* — controls the Steam-overlay input mask below.
- **`PUNKTFUNK_PYROWAVE_CHUNK_KIB`** *(new)* and **`PUNKTFUNK_PYROWAVE_STREAMED_AU`** *(new)* —
  PyroWave AU chunking and the streamed-AU path.
- **`PYROWAVE_QUEUE_PRIORITY`** *(existed, but was inert on Linux — see below)* — grammar: unset →
  realtime, ASCII-lowercased, `off` alone disables, `high` asks for HIGH only, junk falls back to
  the ladder rather than to off. ⚠ **One env var must not mean two things on two platforms**, so
  the Rust grammar is unit-tested against the C patch's, including where both are deliberately
  un-clever (neither trims).
- **`PUNKTFUNK_GAMESCOPE_REFRESH_RATES=60,90,120`** *(new)* — widens the set a gamescope session
  offers in Steam's in-session display settings. The rate the session actually runs at is always
  included, so it can only add options; junk entries are skipped rather than failing the host.
  Requires gamescope patch level 3+.
- **`PUNKTFUNK_COMPOSITOR`** *(behaviour clarified, not changed)* — documented as "which backend to
  drive", it also silently discarded `game_session=dedicated`: `resolve_compositor` gated the
  dedicated route on `!overridden` and logged nothing either way. The pin still wins — it is the
  operator's explicit knob — but it now says so and names itself. Two further holes closed with it:
  the pin put its backend into `available()` unconditionally *and* skipped `apply_session_env`'s
  `XDG_CURRENT_DESKTOP` scrub, so `pick_compositor` could never return `None` — the one call site of
  `try_recover_session()`, which left `PUNKTFUNK_RECOVER_SESSION_CMD` unreachable behind that arm.
  Liveness is now read on both paths. `needs_live_session()` exempts gamescope, which stands up its
  own session, so pinning it on a headless box stays supported.

### Client settings keys

All additive; an older client ignores what it does not know, and a newer value can never trap an
older client.

- **`gamepad_ui_mode`** — `"connected"` (default, and exactly what the previous lone Bool meant) or
  `"always"`. Splits *whether* the controller UI is offered from *when* it appears.
  `GamepadUIEnvironment.isActive` takes the mode with **no default argument** on purpose: a call
  site that forgot it would silently strand everyone who chose Always. An unrecognized value waits
  for a controller.
- **`ui_palette`** gains `oled` at **index 1**, directly after the brand default — keeping
  `PALETTES[0]` the unknown-id fallback and the dark-to-pale cycling order intact. Hand-mirrored in
  three languages (`pf-console-ui`'s `library.rs`, `GamepadPalette.swift`, `GamepadPalette.kt`); each
  port carries an `oled_is_actually_black` test that measures the claim (mean cell luminance 0.019
  against Violet's 0.254) rather than restating the table.
- **`library-hidden.json`** — per-title hide list, mirroring how `library-scanners.json` holds
  disabled sources. Deliberately **not** stored on the entry: a scanner's and a plugin's titles are
  rebuilt from scratch on every scan and reconcile, so a flag written onto one would be erased
  minutes later. Applied in `all_games`, the single funnel every play surface already goes through
  (client grid, native clients, the GameStream app list, launch resolution).

### gamescope patches

Three → six, and the marker patch moves last so the banner is stamped after the capabilities it
advertises.

- **0003 — headless: advertise the virtual display's mode and refresh rates.** `CHeadlessConnector`
  returned empty spans from `GetModes()` and `GetValidDynamicRefreshRates()` and reported
  `GAMESCOPE_SCREEN_TYPE_INTERNAL`, so `update_mode_atoms` **deleted** the mode-list atom and
  wlserver fell through to a one-entry refresh list built from `g_nOutputRefresh` — which, with
  `--nested-refresh` absent, is `Init()`'s 60 Hz default. That is why a 1920x1080@120 client saw
  "gamescope only shows 60hz" and Overwatch capped itself to 60 while the stream ran at 120. Now
  populates both from the resolved mode, reports `EXTERNAL`, and adds `--custom-refresh-rates`.
  gamescope-session-plus has probed for that flag for years; upstream never had it, so the
  `CUSTOM_REFRESH_RATES` env it plumbs was a no-op everywhere.
- **0004 — pipewire: optionally composite the external overlay into the capture stream.** That layer
  is mangoapp. `paint_pipewire` has never referenced it on any version. Behind
  `--pipewire-composite-external-overlay`, off by default.
- **0006 — never destroy the Vulkan device or output.** `g_device` (`CVulkanDevice`) and `g_output`
  (`VulkanOutput_t`) were plain globals, so glibc ran their destructors from `__run_exit_handlers`
  once `main()` returned — calling back into an ICD that had already been torn down and unloaded.
  Faulting address equalling the instruction pointer is the signature. Reproducible with
  `gamescope --backend headless -W 1280 -H 720 -r 60 --xwayland-count 1 -- true` (exit 139, every
  time). Both globals get storage constructed exactly as before but never destroyed; pinning only
  the device relocated the fault into `~VulkanOutput_t`, hence a shared `CNoDestroy<T>`.

  ⚠ **`+pfhdrN` deliberately does not move for 0006.** The marker is a capability tier the host
  probes via `gamescope_patch_level()` *before* it spawns; this patch adds no capability, so bumping
  it would advertise a tier that does not exist. Ships as a `pkgrel` bump instead.

⚠ gamescope CI legs are best-effort — a broken patch is a **missing package**, not a red run.

### Virtual-display handle ownership (Windows)

The control-device sharing contract was "bare `HANDLE` copies, never closed for the process
lifetime": retired handles were kept alive because pinger/linger threads and capture closures held
raw copies whose soundness depended on no-close. An open control handle is exactly what vetoes the
PnP disable — and can wedge the `pnputil` restart — that wake-from-sleep recovery leans on, so every
post-wake adapter reload came back REFUSED. `reset-pf-vdisplay.ps1` stops the whole host service
precisely to get those handles closed; the in-process recovery could not.

Ownership is now `Arc` all the way out: `ensure_device` / `device_handle` / `control_device_handle`
hand out `Arc<OwnedHandle>` clones, every consumer holds its clone across its IOCTLs (ending the
`isize` smuggling — `Arc<OwnedHandle>` is `Send + Sync`), and retiring drops only the manager's
reference. `DeviceSlot::retired` is gone.

⚠ **Nothing may store a bare control `HANDLE` again.** The whole fix is that the handle closes when
the last in-flight user drains.

### Presenter — points are not pixels

`SDL_GetDesktopDisplayMode` reports a mode in **screen coordinates** and hands the pixels-per-point
ratio back separately as `pixel_density`; `m.w`/`m.h` were read raw. KDE advertises a 2560x1600 panel
at 150 % as 1707x1067 points with a density of ~1.4997, `render_scale::apply` even-floors both odd
axes, and 1706x1066 went on the wire. Multiplying by the density recovers 2560x1600 to the pixel.

⚠ Inert on X11 and Windows: SDL never sets a density there and `SDL_video.c` normalizes the unset
0.0 to 1.0. **This bug needed a compositor doing fractional scaling.**

Second, independent defect: the SDL window was created without `HIGH_PIXEL_DENSITY`, so the Wayland
surface stayed at buffer scale 1 and the swapchain was built at 1707x1067 for KWin to upscale. That
one also silently shrank "Match window", which asks the host for `size_in_pixels()`.

### Apple audio session

`micEnabled` and `echoCancel` both default to `true`, so the **default** iOS session is
`.playAndRecord` — and that branch set `.defaultToSpeaker`. That option is an output **override**,
not a preference, and it outranks an A2DP route. ⚠ **Wired headphones beat it, Bluetooth does not**,
so testing with a cable returns the wrong answer — which is what the comment sitting on it asserted.

Now solved against the route actually given: after activation, if the current output is
`.builtInReceiver`, override to speaker; anything external (Bluetooth, wired, CarPlay, AirPlay) is
left strictly alone. The override is a property of the current route — iOS drops it on every route
change, which is what lets a newly-connected headset win — so it is re-applied per route via an
observer, registered only for `.playAndRecord`, removed in `stop()` before deactivate, `deinit` as
backstop. Without it, dropping Bluetooth mid-stream lands on the earpiece.

⚠ Deliberately **not** adding `.allowBluetooth`: it would make a headset's mic usable but drag the
whole route onto HFP/SCO and collapse game audio to narrowband.

### Audio jitter policy

`JitterPolicy` (`punktfunk-core/src/audio.rs`, used by Linux/Windows/Android) and its mirror in
Swift `AudioRing`. The policy learned exclusively from audible failures on both sides: growth needed
**three** audible underruns; the A/V sync loop re-tested a shallower ring every five quiet seconds
and paid an audible starvation event every time it was wrong, forever; and a grown target was never
re-banked (growth raises a threshold — only a re-prime deepens the ring), so a bunching link rode
the knife edge with the "grown" target sitting inert.

Three mechanisms: **near-miss** (a read served with less than one protocol frame left over is the
same evidence as an underrun, heard by no one — grows one step per window, *before* the click);
**shrink probes** (every shrink armed for 5 s, undone on the spot if answered by an underrun or
near-miss, with a doubling backoff 60 s → 8 min on a failed sync-driven shrink; a surviving probe
resets it); **hollow re-prime** (an underrun while the depth *average* runs more than a step below
target re-primes immediately — the average, not the instant, separates a hollow ring from one late
packet, and it is seeded on prime so a fresh ring is never spuriously hollow).

Measured on a ten-minute simulation of the Wi-Fi power-save pattern (25 ms gaps / 300 ms, −50 ppm
skew): **~2000 audible events → 9.**

### Plugins, SDK and the runner

- **`category` never shipped.** The console correctly keeps `category: "library"` plugins out of the
  nav; the host reported no category for them at all. `defineLibraryPlugin` sets it and
  `sdk/src/ui.ts` forwards it — what shipped did not: `@punktfunk/host` was bumped to 0.1.2 on
  2026-07-20 and `category` landed 2026-08-05 without a bump, so the registry's 0.1.2 is the
  pre-category build. ⚠ **Inert until published.** `serveUi` now reads its own directory entry back
  and warns once when a requested category did not land.
- **Local art sync failed on a `file://` disagreement.** `local_art_bytes` decodes a `file://` value
  before testing containment; `validate_art_paths` handed the raw value to `Path::new`. Same defect
  produced both the unreachable settings and `sync (startup) failed: HostRequestError`.
- **The runner now carries SDK updates.** The copy each installed plugin runs was pinned at install
  time, so an SDK fix could never reach it.
- **`bun publish` runs `prepare`, and `prepare` needs bun2nix** — the SDK could not be published at
  all. Also fixed: a corrupt committed `bun.lock` in plugin-kit.
- **Decky client update.** `flatpak remote-info punktfunk-origin io.unom.Punktfunk` names no branch;
  the remote publishes `stable` **and** `canary`, so the ref is ambiguous and flatpak refuses it —
  ⚠ one branch being *installed* does not disambiguate, the ambiguity is on the remote. The call
  failed on every box, every time, and returned `available=False`, which the panel rendered as good
  news. Every query now names the ref in full via `_flatpak_ref()` (no subprocess), carrying the
  **scope** too, so a system-wide install is no longer invisible to a check that hardcoded `--user`.
  A check that cannot run now reports `client_error`.

### Packaging

- **The `punktfunk` group is created everywhere the udev rule needs it.** `60-punktfunk.rules`
  chgrp's the usbip vhci attach/detach nodes to a dedicated group (security review 2026-08-05 M-4:
  writing `attach` materialises an arbitrary emulated USB device, so it must not ride on `input`).
  **Four of six install paths shipped that rule in 0.25.0 without creating the group** — chgrp
  failed, nodes stayed `root:root 0644`, the virtual Deck pad silently never attached, and
  `usermod -aG punktfunk` failed outright. Fixed in arch `post_upgrade()` (only `post_install` was
  correct, so every box that reached 0.25.0 by `pacman -Syu` missed it), nix (`users.groups.punktfunk`
  did not exist), the bazzite sysext (a group is host state and cannot ride an image), and the Steam
  Deck scripts. deb and rpm were correct throughout.
- **`punktfunk-gamescope` now builds for RPM and apt**, not Arch only.
- **Arch release-rebuild prune** called a helper that cannot exist in a release rebuild. Together
  with the FFmpeg 9 repackage this closes the 0.25.0-1 → 0.25.0-2 episode in the pipeline rather
  than by hand.
- **Steam Deck `update.sh` / `install.sh`.** The web step ran `bun install --frozen-lockfile` with
  no `--ignore-scripts`, so web's `postinstall` (`bun2nix -o bun.nix`) rewrote a **tracked** file on
  every update; the SDK step below it had always passed `--ignore-scripts`, and that asymmetry is
  the whole bug. Now `--ignore-scripts` plus an explicit `bun run codegen` — provably equivalent,
  since web's `prepare` is literally `"bun run codegen"` and `src/api/gen`, `src/paraglide` and
  `src/routeTree.gen.ts` are gitignored. `--pull` restores `web/bun.nix` and `sdk/bun.nix` before
  pulling, which is lossless by construction. ⚠ Deliberately **not** `git reset --hard`: `$SRC`
  defaults to the operator's own checkout. Also: `web.env` secret hygiene — `chmod 600` sat inside
  the create-only branch, so an install set up once and only updated since kept it world-readable.
  ⚠ `packaging/debian/build-web-deb.sh`, `packaging/arch/PKGBUILD` and `packaging/rpm/punktfunk.spec`
  still lack `--ignore-scripts` for web — harmless (throwaway build trees), left as follow-up.

### Triage tooling

**`--probe-decode` described a different device from the one that streams.** The RADV
video-decode opt-in sat *after* the `--list-adapters` / `--probe-decode` / `--list-audio` / `--pair`
early exits, so the triage tool never had it. Measured on a Deck, same binary back to back: bare
`--probe-decode` printed "vulkan video decode: no", "driver decode ops: none (0x0)", "no queue
family advertises VIDEO_DECODE"; with `RADV_PERFTEST=video_decode` in the environment, "YES" and
"H.264, H.265, AV1, VP9". ⚠ **Any Deck triage that consulted it reached the opposite of the truth.**
Hoisted to the top of `run`, ahead of every early exit.

### PyroWave on Linux — Wave 2

The program's own measurement, from patch 0005's header: `encode_gpu_synchronous` goes from ~2 ms
to **15–18 ms at 95 % game load**, with the stream frame rate collapsing. PyroWave encodes on the
same shader cores a game saturates; NVENC is immune because it has its own ASIC.

- **PW1 — the GPU-priority lever had never fired on Linux.** The vendored patch requests an elevated
  global-priority queue, gated `if (!inherit_info)` — and **only Windows leaves `inherit_info` null**
  (`pyrowave_create_device_by_compat`, where Granite builds the device itself). Linux passes its own
  create-infos, Granite's `get_existing_create_info()` hands them back, `create_device` takes the
  inherit branch, and the whole block is skipped. Now wired natively in `open_inner`'s `DeviceHold`,
  ladder REALTIME → HIGH → no-priority, stepping only on refusal; a refused class can never fail the
  open. The extension probe reuses the `dev_ext_props` already fetched for `queue_family_foreign` and
  takes KHR or the EXT alias — the same spelling pf-zerocopy probes, so the two cannot disagree.
  ⭐ **Needs `CAP_SYS_NICE`**, which the packaging granted in `0.26.0-1`; without it the lever does
  nothing.
  🛑 **Corrected in `0.26.0-2`: the packaging no longer grants it, and must not.** Every channel that
  did (Arch `.install`, RPM `%caps()`, the Bazzite sysext image, the deb postinst, the NixOS
  `security.wrappers` entry) broke desktop streaming on KDE outright — field-reported on CachyOS and
  Bazzite as `KWin does not expose zkde_screencast_unstable_v1 to this client`. KWin identifies a
  client by resolving its `/proc/<pid>/exe` against an installed `.desktop`, and the kernel refuses
  that readlink to any reader whose effective set is not a superset of the target's **permitted**
  set (`cap_ptrace_access_check`) — KWin has no capabilities, so a capability-carrying host is
  unidentifiable and the restricted globals are never advertised. Neither `prctl(PR_SET_DUMPABLE, 1)`
  nor systemd `AmbientCapabilities=` rescues it; only an uncapped process is identifiable. The lever
  therefore stays wired but unexercised on a stock install (the ladder degrades to default priority),
  and is opt-in for gamescope-only hosts, which have no such identity check.
- **PW5 — two encoder handles.** `Encoder::Impl` owns exactly one each of `wavelet_img_high_res`,
  `bucket_buffer`, `meta_buffer`, `block_stat_buffer`, `payload_data`, `quant_buffer`, and
  `Impl::encode` *opens* by discarding them (an image barrier with `VK_IMAGE_LAYOUT_UNDEFINED` as the
  old layout, plus three `fill_buffer` clears). Two encodes submitted to one queue have **no**
  execution dependency in Vulkan — submission order orders the start, not the completion — so N+1's
  DWT would overwrite N's wavelet bands while N's block packing still reads them. Content-dependent
  and silent. Overlap therefore means two handles alternated, one per slot. ⚠⚠ **The landmine:**
  `sequence_count` also lives on `Impl`, and it is the **3-bit** counter stamped into every block
  header. Two handles each counting 1,2,3… put 1,1,2,2,3,3… on the wire, and the decoder restarts a
  frame only when the value *changes* — so a repeat reads as more blocks of the same frame. Depth is
  **still 1**; the handles alternate with one in flight.
- **PW3 — the fence wait moved out of submit.** PyroWave was the one backend waiting its fence inside
  `submit`.
- **PW7a — the jumbo leg was dead code.** quinn caps a peer's MTU-discovery search at
  `min(MtuDiscoveryConfig::upper_bound, the other side's advertised max_udp_payload_size)`, and
  `EndpointConfig::max_udp_payload_size` **defaults to 1472**. Nothing in the repo had ever touched
  `EndpointConfig`, so raising the host's probe ceiling could never make discovery settle above 1472
  — and the shipped mid-session grow's `settled >= sealed_datagram_bytes(target)` gate was
  unreachable on **every path that has ever existed**. Two smaller contributors fixed with it: the
  watcher stopped sampling the moment `settled >= 1472`, discarding the very climb the proof needs;
  and a session sealed above the 1500-byte default was never checked against the path at all.

  The advertisement is raised on the **client** endpoint under the same `jumbo_wire_mtu()` opt-in,
  because it is not free: quinn sizes its endpoint receive buffer
  `max_udp_payload_size × max_receive_segments × BATCH_SIZE` — on a GRO-capable Linux/Android client
  that is ~2.9 MiB at the default and **~18 MiB at jumbo** (47 KiB → 288 KiB on Apple/Windows).
  PyroWave is the codec that most wants this: it can never be re-keyed mid-stream (its client parses
  chunk-aligned AUs in windows of the `Welcome` value, read once over the C ABI), so it should
  *start* at the big shard. At an 8908-byte shard that is ~6× fewer datagrams per frame — **~49k → ~8k
  pps at 550 Mb/s**.

### Zero-copy capture

- **The dmabuf latch conflated two causes with different lifetimes.** One `AtomicBool` served both
  "the encoder repeatedly failed to import what this compositor allocates" (unrecoverable, a driver
  fact) and "the dmabuf-only capture offer never negotiated" (which can just mean the compositor was
  mid-restart). Sharing it made the second as permanent as the first: **one timeout, and every later
  session on that host captured CPU frames until the process restarted** — including sessions against
  a different compositor and a different node that had never failed at anything, with nothing said.
  Now a `RawDmabufLatch` owning both: import failures stay sticky (unchanged 3-consecutive threshold);
  negotiation timeouts get a retry budget of **2** — deliberately small, since each failure costs a
  ~10 s stall the user pays in dead air; a capture that negotiates credits the budget back; and both
  are keyed to a capture identity (node id + portal bit).
- **The zero-copy path never asked for buffer headroom.** `build_dmabuf_buffers` set
  `SPA_PARAM_BUFFERS_dataType` and stopped — no `SPA_PARAM_BUFFERS_buffers` at all, so the pool depth
  every zero-copy safety argument rests on was entirely the producer's choice and we never expressed
  a preference. Now asks for 8 (min 2, max 16) as a **Choice Range, deliberately not a fixed count**:
  SPA intersects consumer and producer params, so a fixed 8 against a producer that can only afford 4
  empties the intersection and the link stalls in "negotiating" with no error anywhere — ⚠ the exact
  trap that once cost this codebase the entire Linux cursor channel, when a 256² cursor-meta max
  failed to intersect Mutter's fixed 384². 8 buffers is ~133 ms of pool at 60 Hz and ~33 ms at 240 Hz;
  16 is a ceiling, not a request (a 4K 4:4:4 buffer is ~25 MB).
- **A PyroWave session could drop to CPU capture and log nothing.** The CPU-fallback warning was gated
  on `backend_is_vaapi`, which reads the **host-global** encoder pref — but a PyroWave session is
  negotiated **per session**, so on an NVIDIA/auto host that gate is false and the session fell out of
  every arm of the negotiation log chain while paying a full-resolution CPU pixel touch every frame.
  A degraded host and a healthy one produced identical logs. Now asks the per-session question
  (`consumer_kind`), widened to every GPU consumer and excluding only the software encoder, whose
  native input *is* CPU frames. ⚠ `pyrowave_session` must outrank `backend_is_vaapi`, because a
  PyroWave pref flips `backend_is_vaapi` on too.

### Steam-overlay input masking (Steam Deck)

On a Deck in Gaming Mode the Steam menu and the QAM are driven by the **same physical controller** the
client forwards, so opening either moved the game on the host as well — a second, invisible player.
Steam Input masks a normal game here; it cannot mask us, because masking happens on Steam Input's
virtual pad and we deliberately forward the **real** one (the virtual pad has no gyro, trackpads or
paddles).

⚠ **SDL's own gate cannot fire on a Deck.** SDL drops presses while a process has windows but no
keyboard focus, and it is on by default — but gamescope resolves focus per Xwayland ctx and the client
sits alone in its own, so the Steam overlay (which lives in the root ctx) never takes our X focus and
no `FocusOut` is ever generated. Measured on glass: with the QAM open, X input focus inside the
client's ctx stayed on its window for the whole 4 s while `GAMESCOPE_FOCUSED_APP` flipped to 769
(Steam) and `GAMESCOPE_FOCUSED_APP_GFX` stayed on the app. **That pair of atoms is the signal.**

⚠ `overlay_focus` watches them on the gamescope **root** ctx, which is *not* our own `$DISPLAY` under
`--xwayland-count 2` — hence the socket-directory walk and the flatpak filesystem line.

⚠⚠ Masking is deliberately **not** `set_forwarding`: that closes the slot and sends `GamepadRemove`,
so the game would see a controller **unplug** every time somebody opened the QAM. Every slot stays
open and only transitions stop, after flushing what the host believes is held (so a stick deflected at
overlay-open stops steering instead of freezing at its last value). On the way back, held buttons are
**adopted rather than replayed** — the A that picked a QAM row must not fire in the game as it closes
— while axes *are* re-sent, since a stick has no press to ghost and SDL only speaks on change.

### The `plugin` launch kind

The 2026-08-05 review made `launch.kind = "command"` operator-only, and a reconcile refuses on the
**first** offending entry — so rom-manager, whose every ROM is `<emulator> <args> <rom>`, stopped
putting anything in the library at all. Playnite hit the same wall and was rescued with a typed kind
the host resolves itself; there is no fixed scheme for "whichever emulator the operator configured,
with the core and flags they chose", so that trick does not generalise.

The entry now carries an **opaque key and nothing executable**, and the host asks the owning plugin
what to run at launch time, over the loopback UI port and per-boot secret it already registered.
⭐ **A stolen plugin token stops being command execution:** planting an entry is not enough, because
the live plugin answers 404 for a key it never published. Nothing executable is persisted or served to
a client, and an emulator that moved is picked up on the next launch rather than leaving a dead tile
(same reasoning as `xbox` resolving its AUMID at launch time).

⚠ **The host still spawns it**, because only the host can put the process where the stream can see it:
on Linux that is either gamescope's own argv or a spawn carrying the session's compositor env, and the
returned child is what session-game-lifetime tracks to know the game exited. A plugin spawning the
emulator itself would land it outside both.

### Verification status

| | |
|---|---|
| gamescope 0006 | 6/6 exit 0 on a release build at the real spawn shape (`2752x2064@120 --steam --xwayland-count 1`); distro control SIGSEGVs |
| Decky client update | on the Deck against the real install — pre-fix `available=False remote=''`, post-fix `available=True remote=ca010668` |
| `--probe-decode` | on a Deck, same binary back to back, with and without the RADV opt-in |
| Apple audio | builds on arm64-apple-ios17.0 (the triple that compiles the `#if os(iOS)` blocks — a plain `swift build` is macOS and skips them), arm64-apple-tvos17.0, macOS; 257 Swift tests |
| Audio jitter | 10-minute Wi-Fi power-save simulation, ~2000 → 9 audible events |
| 4:4:4 gate | test verified non-vacuous against the planted original defect |
| Steam Deck scripts | `bash -n` + shellcheck 0.11.0 clean at `-S warning`; exec bits preserved |
| Steam-overlay masking | on glass on a Deck — atom flip and X-focus non-flip both measured over a 4 s QAM open |
| PyroWave depth 2 | exercised on real hardware **without shipping depth 2** (dedicated test, shipped depth stays 1) |
| PW6 streamed AU | the trap is real, and at 2 % loss it costs exactly nothing |

⏳ **Owed on glass:** iPhone + Bluetooth listen, Apple TV stats overlay, MacBook audio listen, the
Deck HEVC/4:4:4 retest, a Windows wake-from-sleep cycle, and the PyroWave-under-game-load A/B on a
Linux host with `CAP_SYS_NICE` actually granted — the number this whole wave is aimed at. ⚠ That
last one now needs a **gamescope-only** host, or a hand-granted capability on a box you are not
streaming the KDE desktop from: see the `0.26.0-2` correction under PW1 above.

---

## v0.25.0

407 commits since v0.24.0.

### Versions

| | v0.24.0 | v0.25.0 | Notes |
|---|---|---|---|
| Wire protocol | 2 | **2** | unchanged — every addition below is optional or capability-gated |
| C ABI | 14 | **17** | three steps; see below |
| Workspace crate dirs | 22 | **26** | `pf-bitstream` (+ vendored `cros-codecs`), `pf-vkdecode`, `pf-dxvadec`, `pf-vaadec` added; `pf-ffvk` removed |
| Virtual-display driver protocol | 6 | **6** | unchanged (minimum accepted still 3) |
| Windows virtual-gamepad channel | 3 | **3** | unchanged |
| Plugin index schema | 1 | **1** | unchanged |
| `api/openapi.json` | 0.23.0 | **0.24.0** | tracks API edits, lags one release by convention |

`crates/pf-driver-proto` is byte-for-byte identical to v0.24.0 — if you ship the virtual-display
driver or the gamepad channel, nothing in this release touches you.

**Why the wire did not move.** It grew a lot and still did not break: an optional trailing
`max_shard_payload: u16` on `Hello` (absent/0 = legacy, doubling as the renegotiation capability
flag and the jumbo receive ceiling); two control messages `ShardPayloadChanged` (`0x08`) and
`ShardPayloadAck` (`0x09`); a redundant desktop-audio datagram tag `0xD2` beside the plain `0xC9`; a
controller-audio plane at `0xD1`; a new `0xCD` kind `0x06`; arrival flag bits 8/9; and
`MAX_DATAGRAM_BYTES` 2048 → 9216. Old peers never send or read any of it. Bump `WIRE_VERSION` only
when the handshake or planes change *incompatibly* — riding a C-ABI bump onto the wire once locked
every new client out of every deployed host (`ABI mismatch: client 3 host 2`, observed live).

### C ABI 14 → 17

- **v15 — the rumble policy engine's C surface.** `punktfunk_connection_next_rumble_cmd`,
  `punktfunk_connection_set_rumble_quirks`, `PUNKTFUNK_RUMBLE_QUIRK_*`. These symbols are **not
  new**: they landed while the constant still read 7 and no bump was made, so every core since has
  exported them while advertising a version that never promised them. A shipped binary says what it
  says, so this cannot be corrected retroactively — **v15 is the floor that guarantees them.** At or
  above 15 the surface is present; below it, probe for the symbol. No code changed with this bump.
- **v16 — the controller-audio client surface.** `punktfunk_connection_next_pad_audio` (the `0xD1`
  per-gamepad DualSense haptics/speaker plane), `punktfunk_connection_set_pad_audio_caps`, and the
  `PUNKTFUNK_CLIENT_CAP_PAD_AUDIO` / `PUNKTFUNK_HOST_CAP_PAD_AUDIO` mirrors.
- **v17 — session end reason.** `punktfunk_connection_end_reason` + the `PUNKTFUNK_END_REASON_*`
  vocabulary: after a session ends, ask *why* — this client closed it, the host's launched game
  exited (its close carried `APP_EXITED_CLOSE_CODE`, which the host had been sending for a long time
  with nothing consuming it), the host ended it cleanly, the host reported a failure, or the
  connection was lost. Purely a read of state the core already had: **no new call is required of an
  embedder**, a client that never calls it is unchanged, and the host sends identical bytes either
  way.

### ⚠ Breaking changes

**1. 149 unprefixed macros are now `PUNKTFUNK_`-prefixed** (139 `#define`s renamed in the checked-in
header). Names as generic as `MAX_PADS`, `TAG_LEN`, `ABI_VERSION`, `WIRE_VERSION`, `INPUT_MAGIC` and
the whole `BTN_*` / `AXIS_*` family were landing in the namespace of every program that included the
header.

*What to do:* add the prefix. Values are identical; the change is mechanical.

*It cannot break silently.* The old spellings cease to exist, so this is always an
undeclared-identifier error, never a wrong value — which is precisely the failure being removed. A
colliding `#define` does **not** fail to compile: the preprocessor silently takes the last
definition, so an embedder whose own header defined `MAX_PADS` previously got a wrong value at
runtime. Associated constants are untouched; the generator already qualifies those by type name.

**2. Linux hosts: the virtual Steam Deck controller moved to its own `punktfunk` group.** The
capability rode on `input`, which every gamepad guide tells users to join — but it can emulate
arbitrary USB hardware. Operators must `usermod -aG punktfunk "$USER"` and re-login or the pad stops
attaching. Ordinary virtual gamepads are unaffected.

> **Known issue in 0.25.0, fixed after it.** Four of the six install paths shipped
> `60-punktfunk.rules` — whose `RUN+=` does `chgrp punktfunk` on the vhci `attach`/`detach` nodes —
> without ever creating the group, so the `chgrp` failed, the nodes stayed root-only, and the pad
> silently never attached. The `usermod` above also fails outright on those boxes with *group
> 'punktfunk' does not exist*. Affected: **Arch/CachyOS upgraded** rather than freshly installed
> (`post_upgrade` called only `_ensure_update_group`), the **NixOS module** (no
> `users.groups.punktfunk`), the **Bazzite sysext** (a group is host state and cannot ride an
> image), and **Steam Deck source installs** (`scripts/steamdeck/install.sh`/`update.sh` handled
> only `input`). The deb and rpm scriptlets were correct throughout — they run one `%post`/`postinst`
> on install and upgrade alike. All four now create the group, and the two that know which user
> runs the host (the Deck scripts and the NixOS module's `host.users`) add that user to it as well.
> Workaround on an unpatched box:
> `sudo groupadd --system punktfunk`, then the `usermod`, then re-login.

**3. Plugins may no longer set `launch.command` or the pre-launch command.** Both run through a
shell and are now operator-token only; a plugin that sets them is refused. Third-party plugins that
populated them need updating — use the `launcher_ui` / `xbox` launch kinds instead.

**4. Plugin UIs moved to their own origin** on a second listener (default `PORT + 1`,
`PUNKTFUNK_UI_PLUGIN_PORT`). Reverse proxies and firewalls must forward that port; a self-signed
console needs it trusted separately.

### Capability bits

Four added, all in the handshake's client/host capability bytes:

| Bit | Constant | Meaning |
|---|---|---|
| client `0x04` | `CLIENT_CAP_AUDIO_RED` | can decode the redundant desktop-audio plane |
| host `0x20` | `HOST_CAP_AUDIO_RED` | is sending it |
| client `0x08` | `CLIENT_CAP_PAD_AUDIO` | can render controller audio |
| host `0x40` | `HOST_CAP_PAD_AUDIO` | is sending it |

⚠ **Pressure worth watching:** `client_caps` has four bits free; **`host_caps` is down to its last
one (`0x80`)**; `video_caps` has been full since 0.23.0 (`VIDEO_CAP_MULTI_SLICE = 0x80`). The next
video capability needs a second byte *and* an ABI bump — plan for it rather than discovering it.

### Wire planes

- **Controller audio, `0xD1`** — `[0xD1][u8 pad][u8 kind][u32 seq LE][u64 pts_ns LE][opus payload]`,
  one Opus frame per datagram behind a 15-byte header. `PAD_AUDIO_KIND_HAPTICS = 0` is the pad's
  BACK channel pair (the voice coils) at 5 ms frames; `PAD_AUDIO_KIND_SPEAKER = 1` is the FRONT pair
  at 10 ms. Best-effort like every audio plane: loss is a sequence gap concealed by the gap tracker,
  silence is a frozen sequence under the mic-mute discipline, host gating at −60 dBFS with a 250 ms
  hangover. `0xD2` (redundant desktop audio) deliberately skipped `0xD1` to reserve it for this.
- **`HidOutput::AudioCtl`** — `0xCD` kind `0x06`, carrying the DualSense output report's
  volume/routing bytes, change-only and value-deduped. Older clients drop it as an unknown kind.
- **Arrival flags** — bits 8 (haptics) and 9 (speaker), sent only toward a `HOST_CAP_PAD_AUDIO` host.
- **Adaptive-trigger effects are length-bounded** on encode and decode against one shared constant;
  the header emits `uint8_t effect[PUNKTFUNK_HID_EFFECT_MAX]` in place of a literal `11` (same value,
  so the struct layout is byte-identical). A zero-length effect body is now rejected rather than
  decoding as an empty — that is, a *release* — effect.
- Out-of-range pad indices are dropped before **either** rumble consumer sees them. The reorder gate
  bounds-checked and the legacy queue did not, so an embedder draining it could be handed an index it
  would use to subscript its own array. The client also clamps the host's rumble lease receive-side
  at 5 s, where the ceiling had been sender-side only.

### Host environment variables

| Variable | Default | Notes |
|---|---|---|
| `PUNKTFUNK_AUDIO_QUALITY` | `high` | `low`/`standard`/`high`; `high` = stereo 256 kbps. `standard` reproduces the pre-0.25 encoder exactly for an A/B. A typo warns once rather than silently downgrading. |
| `PUNKTFUNK_AUDIO_REDUNDANCY` | unset = automatic | on when the client supports it and the budget allows |
| `PUNKTFUNK_AUDIO_OUTPUT_MODE` | `client_only` | `client_only`/`host_and_client`/`follow_default`. **Windows host only.** |
| `PUNKTFUNK_PAD_AUDIO` | on | `0` disables controller audio host-wide |
| `PUNKTFUNK_PAD_AUDIO_SLOTS` | `1` | max 4; multi-pad needs an operator to raise it |
| `PUNKTFUNK_PAD_AUDIO_STAMPS` | unset | debug bisect hook |
| `PUNKTFUNK_WIRE_MTU` | unset | pins on-wire IP MTU for all sessions; above 1500 also enables jumbo |
| `PUNKTFUNK_JUMBO` | unset (off) | fixed 9000-MTU profile |
| `PUNKTFUNK_UI_PLUGIN_PORT` | `PORT + 1` | the plugin-UI origin |
| `PUNKTFUNK_LIBRARY_ART_ROOTS` | platform default | art-serving roots; POSIX now defaults to `$HOME` |
| `PUNKTFUNK_DECODER` | client | **values changed**: `native-vulkan` · `native-vaapi` (Linux) · `native-d3d11va` (Windows) · `software`. Legacy `vulkan`/`vaapi`/`d3d11va` still accepted and migrated. Now **trimmed** — a trailing space used to fall through to `auto` silently. |
| `PUNKTFUNK_VAAPI_DEVICE` | client | **new** — pin the VAAPI render node |
| `PUNKTFUNK_DUMP_VIDEO` / `PUNKTFUNK_AU_DUMP` | client | **new** — capture exact decoder input / the AU as it arrived from the host |
| `PUNKTFUNK_AU_FAULT=drop\|truncate\|flip[:period]` | client | **new** — deliberate decoder-input corruption for recovery testing; native rungs only |
| `PUNKTFUNK_NVENC_SPLIT_ARBITRATE=1` | host | **new** — opt-in live split-encode arbitration (Linux-wired) |
| `PUNKTFUNK_NO_AUDIO_MINT` | host (Win) | **new** — opt out of minted endpoints; restores the name ladder |
| `PUNKTFUNK_GPU_PRIORITY` | host (Win) | **removed** — superseded by `PUNKTFUNK_GPU_PRIORITY_CLASS`, a strict superset |
| `PUNKTFUNK_FFMPEG_LOG` | client | **removed** with the av_log machinery |

Legacy `PUNKTFUNK_HOST_AUDIO=1` and `PUNKTFUNK_KEEP_DEFAULT=1` still work, mapping to
`host_and_client` and `follow_default`; `follow_default` wins if both are set. New devtest command:
`punktfunk-host pad-endpoint ensure|remove|status`.

### Security

- **Origin isolation.** A second listener serves `/plugin-ui/**` and nothing else; the console origin
  refuses those paths and the plugin origin refuses everything else, `/api/**` above all. Different
  origin (scheme+host+port) so same-origin policy *is* the boundary; same site so the `SameSite=Lax`
  session cookie still flows. Bind failure disables plugin UIs rather than falling back.
  `x-pf-listener` is stripped inbound and set by the entry; active ports republish as
  `*_PORT_ACTIVE`; the plugin origin's CSP names the console as its only `frame-ancestors`; the proxy
  allowlist drops the plugin's `Clear-Site-Data`, `Access-Control-Allow-Origin` and `Set-Cookie`.
  ⚠ The kit's `postMessage(..., "*")` is **load-bearing** — narrowing it to `location.origin` would
  target the plugin's own origin and drop every message.
- **Authorization is an allowlist with a build-time gate.** `plugin_may_access` is a list of
  permitted `(method, path)` pairs with `{}` segment matching, enforced by a test that walks the live
  route table and **fails the build on any unclassified route** — the block-list it replaces let new
  endpoints through silently. Field authority is tracked separately from route reachability:
  requests carry the lane that authorized them, and `prep` / `launch.kind = "command"` are
  operator-token only.
- **Art serving** gained an extension whitelist plus magic-byte sniffing, canonicalize-or-refuse, UNC
  refusal, config-dir exclusion and root checking, with `file://` percent-decoded *before*
  canonicalization so `%2e%2e` cannot hide. Validation also runs at write time, so an unservable path
  can no longer be persisted.

### Native decode — FFmpeg is gone from the client

268 files, +129k / −25k. `cargo tree -p punktfunk-client-session` finds zero `ffmpeg`. **The host
keeps `libavcodec` unconditionally** (pf-encode); no host workflow, packaging script or licence file
was touched.

| Platform | v0.24.0 | v0.25.0 |
|---|---|---|
| Linux desktop | ffmpeg-next: Vulkan hwcontext (`pf-ffvk`) → VAAPI → libavcodec sw | `pf-vkdecode` (ash, presenter's own `VkDevice`, zero-copy) → `pf-vaadec` (dlopen'd libva, DRM-PRIME dmabuf) → `openh264` + `rav1d` |
| Windows desktop | ffmpeg-next Vulkan → libavcodec D3D11VA half | `pf-vkdecode` → `pf-dxvadec` (plans into `ID3D11VideoDecoder`) → `openh264` + `rav1d` |
| Android | MediaCodec (never had FFmpeg) | unchanged |
| Apple | VideoToolbox (never had FFmpeg) | unchanged |

**Workspace members:** added `pf-bitstream` (+ vendored `cros-codecs`, compiler-enforced
`unsafe`-free), `pf-vkdecode`, `pf-dxvadec`, `pf-vaadec`; removed `pf-ffvk`. **Deleted:**
`video_vulkan.rs`, `video_vaapi.rs`, `video_libav.rs`, the libavcodec half of `video_d3d11.rs`, the
`av_log` machinery, `ffmpeg::codec::Id` as decoder vocabulary, `DecodedImage::VkFrame`/`::Dmabuf`,
the `ffmpeg-fallback` feature, and swscale — and with it the BT.601 default its correction code
existed to undo.

**Software rung:** `openh264 = "0.9"` (BSD-2) and `rav1d = { version = "1", default-features =
false, features = ["bitdepth_8"] }` (BSD-2). `dav1d-sys` was rejected because it is `system-deps`-
only and would add a system library plus a `.pc` to every client package. `default-features = false`
drops `asm` — rav1d's `build.rs` *panics* without nasm, unlike openh264-sys2, which degrades quietly.
**`bitdepth_8` only** ⇒ software AV1 refuses 10-bit by contract, read from the sequence header before
any byte reaches the decoder.

**⚠ HEVC has no CPU floor.** An HEVC session that exhausts its hardware rungs tears down and re-dials
advertising HEVC-less caps, and the host picks H.264 (`last_rung_verdict` / `NoSoftwareRung`). This is
a first-class path, not a failure.

**Rung × codec × hardware evidence** (`native_evidence`) — the admission filter is driven by this, so
an unproven rung yields only to one that is both verified for the codec and usable on the device:

| Rung | Codecs | Evidence |
|---|---|---|
| `native-vulkan` | H.264, H.265 Main/Main10/4:4:4 | **yes** — bit-exact vs libavcodec, 250/250 AUs on 3 drivers + 92-min soak |
| | AV1 | **yes** — 250/250 bit-identical on one vendor, no soak |
| `native-d3d11va` | H.264, H.265 | **yes** — frame-hash parity on RTX 4090 + AMD iGPU, 30-min soak |
| | AV1 | **not proven** — decoded 4K60 once, no parity, no soak ⇒ excluded from the filter |
| `native-vaapi` | H.264, H.265, AV1 | **NO — has never decoded a frame anywhere**; no VAAPI hardware was reachable |
| `software` | H.264 (openh264), AV1 (rav1d) | **not proven**; openh264 has never run on glass. No HEVC at all. |

Vendor order (unchanged): Linux NVIDIA/AMD `vk → vaapi → sw`; Linux Intel/unknown
`vaapi → vk → sw`; Windows NVIDIA/AMD `vk → d3d11va → sw`; Windows Intel/unknown
`d3d11va → vk → sw`.

**AV1 advertisement** now answers from device facts (`av1_hardware_decodable`: Vulkan `DECODE_AV1`
queue op, or the Windows D3D11 import path) rather than `ffmpeg::decoder::find(AV1)`, which was true
on any build linking libdav1d. **Settings migration:** stored `vulkan`/`vaapi`/`d3d11va` migrate to
`native-*` at decoder construction *and* at each dialog's lookup — the second is load-bearing, since
an unmatched value renders as "Automatic" and a save would silently rewrite the preference.

### The three decode data-loss bugs

**AV1 sub-frame truncation — shipped in v0.24.0, host-side.** NVENC sub-frame readback has two halves
armed by *different* conditions: `build_init_params` arms the writer from `subframe_on` alone, while
the chunked reader additionally requires `slices >= 2` — and `resolve_slices` returns `1` for AV1
unconditionally, because AV1 partitions via tiles, not slices. So an AV1 session told the driver to
publish tile-by-tile and then took only the first tile. Measured at 4K60: every AU carried a header
declaring two tile rows plus a single Tile Group OBU with `tg_start = tg_end = 0`; libdav1d rejected
**835/836** AUs. NVIDIA's *hardware* decoder accepts it (so Vulkan Video looked healthy at 60 fps);
its DXVA path did not. 1080p is one tile and unaffected; 4K splits into two tile rows and loses half
the picture. Fixed by disarming sub-frame for AV1 while leaving `split_mode` untouched — AV1 keeps
every engine. Arming the reader instead is *not* a drop-in: the reader cuts at
`bitstreamSizeInBytes` on the reasoning that slices are contiguous Annex-B, which AV1 OBUs are not.
Post-fix 654/654 clean. The test that had pinned the old behaviour as *correct* is replaced by one
pinning the disarm, plus one comparing the reader's gate against the writer's — the comparison
nothing made.

**HEVC DPB from the level ceiling — new in this release, client-side.** `dpb_limit` computed
`max(A-2_level_ceiling, sps_max_dec_pic_buffering_minus1 + 1)`. HEVC equation A-2 is a **ceiling on
what an SPS may legally signal**, not a statement of need, and it branches on picture size against
the *level's* `MaxLumaPs`. The host is blameless: NVENC autoselects L5.1 because the bitrate exceeds
L5.0's ceiling, and signals six pictures at every resolution. At 720p and 1080p the A-2 branch yields
16 frames / **17 slots** — one more than NVIDIA's `maxDpbSlots` of 16 — so every AU fell outside
device caps, flushed, waited for an IRAP, and the fresh IDR needed 17 again; rungs exhausted, and
there is no software HEVC. It hid because the path was only ever exercised at 4K, the one size that
falls through to the honest answer. Fixed to `buffering.min(16)`: the `max()` bought no tolerance,
since `Dpb::needs_bumping` already evicts at the signalled depth — it only over-allocated ten
surfaces per 1080p session. **H.264 escaped by luck** (its ceiling lands at 13 for 1080p) and is left
alone, because H.264's DPB size genuinely *is* level-derived absent a VUI `bitstream_restriction`.

**rav1d aborts the process — new in this release, client-side.** rav1d 1.1.0 `abort()`s on *any*
decode error while holding one frame context: the `c.fc.len() == 1` branch decodes inline, always
finishes in `rav1d_decode_frame_exit` which unconditionally takes `frame_hdr`, then on `Err` re-enters
an `on_error` whose first act is `frame_hdr.as_ref().unwrap()` on the `None` it just left. The panic
unwinds into `dav1d_send_data`, which is `extern "C"` ⇒ `panic_cannot_unwind` ⇒ `abort()`. **No
`catch_unwind`, no rung demotion and no refusal can catch it**, and every `rav1d_*` entry is
`pub(crate)`, so no in-process guard is possible. 4K was only *where* the first error happened — the
CPU rung does 35–39 fps against a 60 fps stream, the backlog stopped draining, the pump flushed to
live, and the next AU referenced undecoded frames. Fixed by opening with `n_fc >= 2` and asking
`dav1d_get_frame_delay` what the settings actually bought. Decode now drains **past** the first
`EAGAIN`, which is why two frame contexts cost no latency (20–42 ms/unit at `n_fc=2` vs 21–53 at
`n_fc=1`). On glass: 4K60 AV1 was SIGABRT on the second frame every run; after, exit 0 with 1204
frames and 13 decode errors recovered across 17 backlog flushes. Reported upstream as **rav1d#1497**
with a reproducer. Does **not** make the CPU rung panic-proof.

**Settings loader BOM — shipped in v0.24.0, client-side.** `.and_then(|s| from_str(&s).ok())` turned
every parse failure into `Default`. `Set-Content -Encoding UTF8` writes `EF BB BF`, serde_json
correctly rejects at byte 0, and every setting vanished silently. A shared `load_json_or_default` now
strips the BOM and warns with path plus serde line/column, covering settings, known-hosts (where a
BOM silently unpaired every host) and profiles on both desktop clients. The result is deliberately
still `Default`, never an error.

### Other decode/encode

- **Intel Arc pNext ordering.** `vkGetPhysicalDeviceVideoCapabilitiesKHR` was called with the codec
  caps struct chained *before* `VkVideoDecodeCapabilitiesKHR` (`push_next` prepends). Arc/Windows
  fills those two **by position, not by sType**, and returned them swapped — we read a level as a
  capability bitmask. Measured A/B: `decode_flags_raw=12 max_level_idc=1` before,
  `decode_flags_raw=1 max_level_idc=12` after. NVIDIA and RADV dispatch by sType, which is why the
  fleet stayed green. ⚠ **This does not yet give Arc Vulkan Video** — the refusal only moves down: the
  device advertises only COINCIDE, and its NV12 coincide entry does not advertise `SAMPLED` usage,
  which the zero-copy presenter needs. Unresolved whether that is ours or an Intel constraint.
- **NVENC split encode.** The 10-bit rule sat *above* the pixel-rate arm and took no codec, so it
  vetoed 10-bit 4K120 — the exact case the pixel-rate arm exists for — and applied an
  HEVC-Main10-on-Ada result to AV1 10-bit, which has no such measurement. Re-measured on Ada and
  Blackwell: 4K60 2.06×, 5120×1440@240 1.31×, 4K120 1.89× — **split wins at every mode on both
  architectures, including the configuration the veto came from.** New order: env override →
  pixel-rate arm (now taking `max_forced_split_mode(engines)`, not a hard-coded 2) →
  HEVC-Main10-below-the-bar → AUTO. Operator over-asks are clamped with a warning because **the driver
  honours an over-ask and silently encodes narrower**. Also newly logged: HEVC + plain AUTO +
  sub-frame is **silently single-engine** — the fleet's default shape, and nothing said so.
  ⚠ **Unvalidated consequence:** 5120×1440@240 Main10 now clears the pixel-rate bar and *will* be
  forced to split — the exact configuration the old veto came from. `PUNKTFUNK_SPLIT_ENCODE=0` is the
  escape.
- **PyroWave on Windows stamped over the host's GPU scheduling policy.** It raised the process WDDM
  class to HIGH at every session open, while `auto_priority_gate` already owns that process-wide —
  starting at HIGH, *upgrading* to REALTIME once safe, and leaving a monitor that drops back when VRAM
  tightens (REALTIME + NVIDIA + HAGS + near-full VRAM is a documented NVENC hang). Opening PyroWave
  stamped HIGH back and **orphaned the monitor's decision**. Removed rather than reconciled.
- **A `pf-vkdecode` AV1 use-after-free fix had stabilised the wrong pointer** —
  `OwnedStdAv1SequenceHeader` kept the Std struct *inline*, so `pStdSequenceHeader` was a dead stack
  address; it worked only because NVIDIA happened to retain `pColorConfig` instead. Std structs are
  now boxed inside each owning wrapper, and create-time arrays are fields of the stored parameters
  assembled at their final address. The same shape was fixed pre-emptively in H.264/H.265.

### A/V sync — it did not previously exist

The host has always stamped `pts_ns` on every audio datagram. **Every client decoded it into
`AudioPacket` / `AudioPCM` and never read it.** Video's `pts_ns` was used end to end; audio free-ran
at whatever depth its jitter ring reached; nothing compared them. The A/V offset was an emergent
property of buffer depths — it moved whenever the ring ratcheted under underrun pressure, and it got
**worse every time video got faster**, because a quicker decoder lowers the video leg and leaves
audio's where it was. That is why shaving milliseconds off the audio budget had never helped.

Two host defects were prerequisites:
- **`pts_ns` was stamped at encode time**, inside the loop draining an already-accumulated chunk, so
  every frame of a chunk carried near-identical timestamps describing *when we got round to
  encoding*. Now derived from the chunk's arrival instant minus queued-frame duration, re-anchored
  per chunk.
- **The host did not pace.** One capture callback hands over a whole quantum (5 ms honoured, **21.3 ms
  on a VM**, where stock PipeWire raises `min-quantum` to 1024), drained into back-to-back
  `send_datagram` calls — a 4–5 frame burst then ~21 ms of nothing, which a ring could only absorb by
  standing a burst period deep. Frames now leave on the audio clock (`FRAME_INTERVAL` 5 ms,
  `PACE_MAX_SLEEP` 10 ms, `PACE_REANCHOR` 100 ms). Costs no average latency.

```
audio_e2e = (now + buffered_ahead + clock_offset) − pts_ns
av_offset = audio_e2e − video_e2e            (> 0 ⇒ audio behind the picture)
```

`AvSync` EWMAs it (`AV_EWMA_TAU_MS = 2000`), ignores anything inside `AV_DEADBAND_MS = 10`, waits
`AV_MIN_OBSERVATIONS = 100` before a first correction, and **refuses rather than clamps** beyond
`AV_SANE_LIMIT_MS = 1000` — a wall-clock step must not steer the ring.

⭐ **Video is the master, and continuity outranks sync.** `JitterPolicy::set_sync_target` takes only a
*request*, clamped between the existing underrun-driven adaptive floor and the hard cap: a link whose
jitter genuinely needs more buffer than the picture is away keeps its buffer, and the residual is
reported rather than forced. `None`/`nil` reproduces prior behaviour bit-identically, which is how
the four rings adopted it one at a time.

Per client: the Rust desktop reference is a new `video_e2e_ns` atomic beside `clock_offset`, written
by the presenter and read by the audio thread. **Android** publishes `OnFrameRendered` — the one
place that knows a frame *latched* — **raw, not floor-shaved** (the HUD shaves the OS present floor;
sound must reach the ear when light reaches the eye), and stays inert below API 33 rather than
substituting the release instant, which targets a future vsync 8–21 ms ahead of glass. **Apple**
publishes its `LatencyMeter` sample as an *expiring level*, because that client has a backgrounded
keep-alive that keeps audio playing while dropping video decode; its clamp raises the ceiling to the
floor rather than `min(max(…))`, which on a device whose callback quantum alone exceeds the hard cap
would otherwise hand back the cap, silently below the continuity floor.

Escape hatches: `PUNKTFUNK_NO_AV_SYNC=1` everywhere, plus
`adb shell setprop debug.punktfunk.no_av_sync 1` on Android (a launcher-started app inherits no
environment). Observability: `buffer_ms`/`target_ms` had only ever been a `tracing::debug!` line —
and on a Deck the client runs under Steam's `reaper` with stdout on a pipe nobody can read, so the
one number identifying a deep ring was unobtainable *on the device reporting the latency*. Now on the
HUD and in the 1 Hz stats log on every client.

### Decode-target aliasing — caught before it shipped

⚠ **None of this ever shipped.** `git ls-tree v0.24.0 crates/` has no `pf-vkdecode`, `pf-dxvadec`,
`pf-vaadec` or `pf-bitstream`; v0.24.0's decode rungs were libavcodec. This was a ship-blocker for
the new stack, cleared — not a field bug.

Three of the four native rungs released a picture's surface **inside the plan→submission
conversion**, then assigned the decode target a slot. `SlotMap::assign` returns the *lowest free
slot* — the one just vacated. The submission then named one surface as both decode target and its own
reference: `CurrPicTextureIndex == RefFrameMapTextureIndex[k]` on DXVA, or `pSetupReferenceSlot`
sharing an array layer with `pReferenceSlots` on Vulkan. **Decode into the surface you are predicting
from.**

- **AV1 / D3D11VA** — AV1 applies `refresh_frame_flags` *after* decode (7.20), so "read a slot then
  overwrite it" is the ordinary case: **268 of the vendored vector's 274 frames**, first at frame 6.
- **H.264 / both Vulkan and D3D11VA** — `H264Planner` snapshots `dpb_refs` in `begin_picture`, before
  8.2.5 marking and the C.4.5.3 bump, so a picture the sliding window unmarks and the bump evicts
  lands in *both* `dpb_refs` and `dpb.removed`. Both conditions coincide only in low-delay H.264 —
  and NVENC guarantees it (`max_num_ref_frames = 3` alongside `max_dec_frame_buffering = 3`, plus
  `max_num_reorder_frames = 0`). Result: **297 of every 300 access units of every stream a punktfunk
  host emits**, at every resolution, on both rungs.
- **H.265 is exempt, now measured rather than argued** — 0 of 120 aliases, with a counterfactual that
  moves the snapshot one call earlier and reproduces 115 of 120.
- **VAAPI's exemption was incidental**: the precondition is fully present (117 of 120 AUs) but
  `plan_to_va` never invents a surface. That held only because three call sites happened to write
  `free_surface()` and `surface_table()` adjacently; `acquire_target` now returns index, surface and
  table together so a later edit cannot split them.

Fix is uniform: the plans grow `release_after_decode`, conversions hand removals back, callers
release once the decode op is issued. Costs no slot (`SlotMap::new` allocates `max_dpb_frames + 1`).
Both rungs hold the `Result` rather than `?`-ing it so the deferred release runs on failure paths —
seven exits sat between conversion and release, each of which would have leaked a slot.

**Why four gates missed it**, all recorded: the conformance vector is *structurally blind* (level 1.3,
no VUI `bitstream_restriction` ⇒ a 7-frame DPB against 2 reference frames, and it reorders) and
passed 250/250 for two milestones; **a test had encoded the bug as correct**; another assertion was
*vacuous* (it asserted the decode target was never also a reference while handing every picture its
own never-reused surface id — distinct integers cannot collide); and **it streamed clean** — *"the
2026-08-07 field sessions that looked clean were looking at wrong pixels."*

`gpu_parity` is now **11 legs** (not 9 — that note was written mid-PR): each decodes a vendored stream,
reads back every output frame's NV12, crops to the display region and SHA-256s in *display order*
against libavcodec goldens, frame count and flush tail included. The three new legs are our own
encoder's output rather than conformance vectors — H.264 because the vector is blind to the shape,
H.265 because an exemption with no stream behind it is how the H.264 defect survived two milestones,
AV1 because the vector is one tile on all 274 frames while our encoder splits 4K into two tile rows,
so every tile array the conversions fill had only ever been written at index 0. `video_vaapi_native`
parity is new entirely: 7 legs, bit-identical on RDNA3.

⚠ Promoting D3D11VA AV1 to `verified` **changes rung selection** on Windows Intel/unknown vendors, not
just a label. VAAPI stays `verified = false` deliberately — one vendor, never soaked; flipping it
would move `auto` off Vulkan Video on every Linux AMD/Intel client including the Deck.

### FFmpeg 9, and the Arch soname trap

`pf-encode` now builds against **FFmpeg 9**. The host still links libavcodec unconditionally; the
client has none (see above).

⚠ **`pacman` is the only one of our packaging formats that does not derive dependencies from ELF
`DT_NEEDED`.** rpm auto-generates `libavcodec.so.62()(64bit)`, `dpkg-shlibdeps` emits `libavcodec62`,
nix pins the closure — but a bare `depends=('ffmpeg')` let `pacman -Syu` walk the host across a
soname bump with no warning and no conflict. FFmpeg 8 → 9 (`2:9.0-5`: libavutil .60→.61, libavcodec
.62→.63, libavfilter .11→.12, libavdevice .62→.63, libswscale .9→.10) therefore **bricked every
Arch/CachyOS install**: the dynamic loader cannot start the binary, so it is **exit 127 before
`main()`** in a systemd restart loop, with nothing in the host's own log to explain it.
`ldd /usr/bin/punktfunk-host | grep "not found"` is the one-line diagnosis.

⭐ The fix is **SONAME deps, not a hand-written version bound**: `depends=(… 'libavcodec.so'
'libavutil.so' …)`. Arch's ffmpeg declares matching `provides=(libavcodec.so=63-64 …)`, and makepkg
rewrites each bare `libfoo.so` into `libfoo.so=<soname>-<arch>` by reading the built binary's
`DT_NEEDED` — so the bound tracks whatever FFmpeg the builder linked against with nothing to
maintain across the next bump. A literal `ffmpeg<2:9` would go stale on every bump. pacman now
refuses the upgrade instead of bricking the install. All seven libs are listed even though
`--as-needed` currently drops two: an unlinked soname is left bare by makepkg and satisfied by any
ffmpeg, so listing it costs nothing and a future link picks up the bound automatically.

🛑 **The v0.25.0 Arch packages shipped with that bound pointing at the WRONG FFmpeg — install
`punktfunk-host 0.25.0-2` or newer.** The soname fix and the FFmpeg-9 build landed as one merge;
the release tag was pushed four minutes later, while the CI builder image was still being
rebuilt. arch.yml deliberately runs no `-Syu` ("the image's snapshot IS the build environment"),
so the release was linked against FFmpeg 8 and published `libavcodec.so=62-64` — a bound no
up-to-date Arch box can satisfy. It fails *safely* (pacman refuses; nothing bricks), but it fails
**loudly and broadly**: pacman prepares one transaction, so an unsatisfiable dependency of ours
stopped affected users' entire `pacman -Syu`. `0.25.0-2` is the identical source rebuilt against
FFmpeg 9. Only Arch was exposed — every other format derives its dependency from the ELF at build
time and could not disagree with itself this way.

Two guards now stand where only a convention did. arch.yml compares the builder's libav
`provides` against the live repos before building and `-Syu`s itself if they differ; and no
package is published until a **pristine-`--dbpath`** `pacman -U --print` resolves it, which asks
"would a real, up-to-date Arch box install this?" instead of "does the builder happen to satisfy
it?" — the distinction that let this ship. Keeping `ci/arch-ci.Dockerfile` current is still the
cheap path; the guards are the backstop.

### Linux playback filled the buffer ceiling

The PipeWire playback callback sized its writes from the mapped buffer's **capacity** — PipeWire's
quantum limit, 8192 frames ≈ 170 ms — instead of the graph's per-cycle ask (`pw_buffer.requested`).
Every cycle queued up to 170 ms of PCM downstream of the ring **and** taught `JitterPolicy` that the
device drains 170 ms per callback, so the underrun floor (want + one frame) rose above any depth the
A/V sync loop could request: sync measured audio ~280 ms late and was then forbidden — **by its own
continuity rule** — from draining it. The first on-glass run of the latency overhaul showed exactly
that: `audio buffer 272 ms, a/v +284 ms`, stable. Now honours `requested` (capacity remains both the
ceiling and the fallback when `requested == 0`) and logs requested-vs-capacity once per stream.
Needs libpipewire ≥ 0.3.49; every ship target clears it.

### Windows audio substrate

The host now mints its **own** devnodes from Valve's INFs (`SteamStreamingSpeakers.inf` /
`SteamStreamingMicrophone.inf` under `{CommonProgramFiles(x86)}\Steam\drivers\Windows10\…`) instead
of bundling VB-CABLE.

- **Two persistent endpoints**, `Punktfunk Speakers` (client-only loopback sink — the wiring plan
  parks the default playback on it during a stream, its WASAPI loopback feeds the encoder, the host
  stays silent) and `Punktfunk Microphone` (host writes decoded client voice into the render side;
  the capture side surfaces as the mic). Both survive host restarts and re-resolve by marker.
- **Identity is the recorded endpoint id, never the name** — a minted instance is name-identical to
  Steam's primaries. Durable marker `PunktfunkAudioRole` (1 = Speakers, 2 = Mic) under Device
  Parameters. Name stamping is device-desc + device-name **only**: a wider stamp set makes
  `AudioEndpointBuilder` re-mint under a new GUID. Best-effort via the SYSTEM ACL route; on failure
  the endpoint still wires and simply keeps the driver's default name.
- **Format stamps are per-direction.** Render gets the PCM16-device / float-mix stereo split; capture
  gets the **device-format key only** — mix and host-format keys are render-engine properties, and
  stamping them onto a capture endpoint breaks its shared-mode graph (`IsFormatSupported` reports
  2ch/48k fine, `Initialize` then fails `0x88890008`).
- **`MintedIds` is tier-0 in the wiring plan.** The mic takes its minted device outright (paired by
  provider id — a name search cannot distinguish it from the primary); the loopback prefers the
  minted sink at the head of the silent tier. Below that the old ladder is unchanged: Steam primaries
  → cable → real hardware. `PUNKTFUNK_MIC_DEVICE` still beats everything.
- **Mic-vs-loopback arbitration**: the mic may hold the Streaming Microphone only while the loopback
  still gets a non-last-resort pick; otherwise the loopback takes it and `mic_withheld` is set. This
  fixes a field case where a headless Steam-only host streamed **silence**.
- **New `AudioReadiness`** — `Full` / `AudioOnly` / `MicOnly` / `Nothing`, logged on every plan
  change and surfaced at `GET /api/v1/status` → `RuntimeStatus.audio` (`AudioWiring`, Windows-only,
  absent before the first wiring pass; a status poll triggers no COM work or `IPolicyConfig` writes).
  The console Dashboard renders it as an "Audio wiring" card.
- **Requires Steam installed** (never running) — without the INFs the host streams video only, and
  picks the drivers up automatically if Steam is installed later. Opt out entirely with
  `PUNKTFUNK_NO_AUDIO_MINT`, which restores the previous name-based ladder exactly.
- ⚠ **VB-CABLE is no longer bundled but is deliberately NOT uninstalled** — it is a third-party
  shared component other apps may use, and it stays in the ladder as a live fallback. Demoting it was
  considered and rejected: on a box where minting transiently fails, that would let the Steam
  Streaming Microphone outrank an installed cable, steal the silent sink and make stream audio
  audible on the host.
- ⚠ **The minted endpoints survive Punktfunk's uninstall by design** (they are plain instances of
  Steam's drivers and are inert without the host). There is no user-facing removal path; cleanup is
  the devtest `punktfunk-host audio-probe cleanup`.
- New devtest: `punktfunk-host audio-probe ssm|sink|sss-primary|mint|plan|micpitch|micpins|cleanup`.
  `plan` is the field-triage command; `micpins` maps exclusive+shared `IsFormatSupported` across
  {1,2}ch × {16,32}bit × {44.1,48,96}kHz on both mic pins.

### Apple audio

- **The microphone was never in the render graph.** On the combined (voice-processing) engine — made
  default a week earlier and never run on a device — the input node carried a tap and **no
  connection**, so nothing pulled it: the IO unit came up, the recording indicator lit for a beat,
  and not one buffer ever reached the tap, with no error and no failed start. The 10 s silence
  tripwire counts *captured* frames, so it never fired. Input now runs through a silent sink into the
  main mixer at `outputVolume = 0` (Apple's own voice-processing sample topology). Two more: the tap
  read the input format **before** `prepare()`, and enabling voice processing swaps in the VPIO unit
  and renegotiates, so the pre-swap read could be 0 Hz / 0 ch; and a mic-chain failure on the
  voice-processed engine took the whole uplink down for the session — it now falls back to the split
  path, because **the mic outranks the AEC**.
- **No packet-loss concealment on the one client that decodes Opus in core.** Linux, Windows and
  Android all feed an `AudioGapTracker` and synthesize libopus PLC; the in-core path had the tracker
  sitting unused in the same crate and decoded only packets that arrived. At ~200 packets/s of 5 ms
  frames every lost datagram was a hard time-domain gap — one click per loss. The redundant plane
  (`0xD2`) hides single losses, so the survivors were exactly the burstier gaps that most needed
  concealing. Concealed frames now land in front of the arriving frame in one contiguous buffer, a
  DTX marker advances accounting without being decoded, and the output buffer is pre-sized for a full
  concealment run so the borrow-until-next-call pointer cannot dangle (50 ms cap).
- **The Apple jitter ring never grew.** The shared Rust `JitterPolicy` has an adaptive target floor;
  the hand-written Apple mirror mirrored the *shed* half but not the *growth* half, pinning its
  target at the 20 ms base forever. On Wi-Fi that bunches arrivals, 20 ms is regularly shorter than
  one delivery stall, so the ring re-primed through every stall for the whole session. Now the full
  `note_read` mirror: 3 underruns in a 5 s window grow the target 10 ms (capped at CoreAudio's 70),
  30 s of quiet steps back, and the write-side hard trim follows the grown target.

### Clients

- **Nothing in the desktop console had ever been clickable.** `SkiaOverlay::handle_event` matched
  only `KeyDown` and `TextInput`, so every mouse button, wheel and touch contact fell past the console
  into the run loop, which routes pointer input exclusively at `stream.capture` — `None` while
  browsing. New `Overlay::handle_pointer` carries mouse/touch in swapchain pixels; the run loop
  converts (it owns the window and hence display scale); the console hit-tests the rects it drew last
  frame. Only **direct** touch devices are offered — an indirect trackpad already drives the mouse.
  Widgets act on **press**, not release, because both carousels scroll the focused item toward centre
  and what you pressed would slide out from under your finger. Host menu on Up from a saved tile;
  `UpdateHost` edits **in place** (remove-and-re-add would silently drop the fingerprint, learned MAC,
  pinned cards and profile binding), and `ForgetHost` arms on first press and fires on second.
- **Discovery went permanently deaf three ways**, each needing an app relaunch: a failed resolve was
  never retried (`browseResultsChangedHandler` fires only when the result *set* changes, and a host
  whose resolve failed is still in the set); a stuck resolve never ended (`NWConnection` has no
  timeout, so the throwaway UDP flow could sit in `.preparing` forever, and a service with a
  connection in flight was skipped); and an `NWBrowser` parking in `.waiting` was ignored — **which is
  exactly where iOS's local-network privacy prompt lands on first launch, and granting it does not
  revive the browser that was already waiting.** A 1 Hz sweep now times out stuck resolves, retries
  failed ones on a 1→30 s backoff, and re-arms a dead browser; the advert's TXT is re-read on every
  browse report. `discovery::Rescan` forces a fresh mdns-sd query — the browse otherwise re-queries on
  a doubling backoff **capped at one hour**, so a long-lived browse is effectively passive. ⚠
  `clients/windows/src/discovery.rs` is a **second copy** of the browse that the earlier IPv4 pinning
  missed; it took an arbitrary first address, so a host whose OS responder answered AAAA rendered a
  card that failed on every click.
- **Phone gyro mirror**, off by default, player 1 / wire pad 0 only, and only while that pad has no
  motion source of its own. iOS/iPadOS only on Apple (`DeviceGyro` wraps `CMDeviceMotion` at ~100 Hz
  on a dedicated serial queue — the controller path's main-queue delivery is a known jitter source);
  Android phones with a gyroscope at ~200 Hz with `maxReportLatencyUs = 0`, since batching is poison
  for gyro aim. Both rotate from the device's natural frame into the controller frame by interface
  orientation, and both send **one zero-gyro sample on stand-down** — the host holds motion as state
  and re-emits it, so a leftover nonzero angular velocity reads as endless rotation.
- **Safe-area resolution** is purely a *sizing* change — no layout change, no input change; pointer
  mapping follows for free since both clients derive the picture rect from the live host mode. Full
  native height, width less left+right safe insets. Portrait settings screens report the housing on
  `top` with zero horizontal insets, so the portrait top inset stands in (gated so an iPad's status
  bar never fabricates one). Android adds the rounded-corner radius, which it does not count as
  cutout. Both even-floor and clamp, because `validate_dimensions` rejects odd dimensions and an inset
  subtraction lands odd about half the time.
- **Gamepad UI**: six sections (Stream · Video · Audio · Controller · Interface · Profiles, plus Input
  on the desktop console) walked with L1/R1 with per-section cursor memory; 12 palettes under one
  shared `ui_palette` key, Violet keeping its explicit sixteen colours so existing installs are an
  identity transform. Presentation only → **device preference, never part of a profile**. Palette
  maths ported three times (Rust/Swift/Kotlin) with the same assertions pinned in each language;
  `every_palette_is_multi_tone` fails under 45° hue spread and caught Ember at 35° and Graphite at 3°.
  Three render-only findings: additive blending blows out over a pale ground, a white scrim at the
  dark field's strength bleaches the gradient, and white glass over a bright field needs more body.

### Session and game lifetime

- **`PunktfunkEndReason` replaces a single "closed" bit** (ABI 17, additive, wire untouched). Five
  values — local, game exited, host ended, host error, lost — classified by the connection watcher
  from close codes already on the wire (`APP_EXITED_CLOSE_CODE` had been sent for a long time with
  nothing consuming it). **Latched before the shutdown flag**, because the two are read by different
  threads and the reason must never arrive second. Exposed as `punktfunk_connection_end_reason` +
  `is_normal()`. Shells fall back to the old wording when there is no verdict (older core, or a close
  that raced the read).
- **The Steam `Running` registry hint was an unbounded veto.** Honouring it reset the absence window
  every pass, so a flag Steam left set — Steam crashed, was closed first, the game re-parented —
  pinned a lease in `running` for the life of the host process. The absence timer now runs
  regardless; past `VETO_LIMIT` (30 s) with nothing of the game on the box, the session ends anyway
  and logs at WARN. Extracted as a pure `exit_confirmed(gone_for, hint_running)` with tests — the
  watch loop polls a live process table and cannot be unit-tested, which is exactly how the
  unbounded veto shipped.
- **New `launchreg.rs`: one record per `(client fingerprint, library id)`**, written at launch and
  independent of the termination policy. The old fingerprint-keyed reclaim only ran under
  `GameOnSessionEnd::Always`, so under the default `Keep` nothing was recorded — and a client retry
  re-sent `Hello::launch` verbatim, which the host obeyed unconditionally. Steam/Epic URIs hid it
  (the launcher just focuses the running copy) but a `gog:`/`custom:` target genuinely started a
  second instance over the same save files. The same retry also minted a fresh `launch_stamp`, so
  procscan refused to adopt a game older than 2 s and **a reconnected session lost game-exit
  detection for the rest of its life.** Identity now flows backwards from the watcher, which
  publishes the concrete `ProcRef`s it adopted; liveness is `Scanner::alive` over that recorded set,
  re-verified by `(pid, start)`. Tradeoffs: a `custom:` command with no detection hints stays
  `Unknown` forever (trading exit detection for not double-spawning), and `IN_FLIGHT_WINDOW` is a
  fixed 90 s, deliberately not `disconnect_grace_seconds`.
- **A launcher entry is `LeaseKind::Untracked` unconditionally**, checked ahead of
  `nested`/`child`/`spec`. Its lifetime previously depended on invisible state: launcher not running
  → live child → `Child` lease → quitting the launcher ended the session; launcher already running →
  command forwards and exits inside `SHIM_WINDOW` → `Untracked` → session persists. Steam Big Picture
  is a *mode*, not a process (and on a Deck it is always running); Heroic is single-instance
  Electron. The real trap was the GameStream path, whose `GsApp` intermediate silently dropped the
  field.

### Library and plugins

- **Store claims keep identity across the scanner-to-plugin handover.** `library.json` gains a v2
  `{entries, claims}` shape that reads the old bare array unchanged and rewrites on first mutation.
  `PUT /library/provider/{p}?store=<s>` claims a store; entries then surface as
  `<store>:<external_id>` rather than `custom:<id>`, so entry ids, GameStream app ids, client art
  caches and Moonlight pins all survive. One provider per store (409 otherwise); while a claim is
  held the matching built-in scanner is skipped, so the two never double-list.
- `GET/PUT /library/scanners` is now a **sources** endpoint over the same disabled-set file.
- New entry fields: `role: game|launcher`; launch kinds `steam_ui` (`bigpicture|desktop`),
  `launcher_ui` (platform-gated, 400 on invalid) and `xbox`.
- **Plugin kit 0.3.0** adds a `./library` subpath: `defineLibraryPlugin` plus ported total parsers —
  text VDF/ACF, the binary `shortcuts.vdf` walker with CRC-32 appid derivation, read-only immutable
  SQLite, a registry wrapper that refuses HKCU, path-confinement joins. `GET/PUT /__config` returns
  `{schema, value}` and persists raw, so a plugin with settings need not ship an SPA.

### Platform and packaging

- **The client's config writer** falls back to an in-place write when the atomic replace is
  unavailable, verifies it by reading the bytes back, and records the last persistence failure
  centrally so the UI can surface it. Scratch files are now per-process, closing a real collision
  between the five processes that write these stores (shell, session, console UI, CLI, Decky) — one
  could previously rename its half-written temp over another's target.
- **Host send pacing** gained a pure, unit-tested budget function: oversized frames are budgeted at
  the pacing rate with a 100 ms absolute ceiling rather than compressed into one frame interval.
  Steady-state schedules are byte-identical, the legacy behaviour stays reachable via an environment
  escape hatch, and the GameStream-compatible path is untouched.
- **Mid-session shard renegotiation is gated off for PyroWave sessions**, which parse the video
  stream in windows fixed at session start — re-sizing mid-stream would corrupt the parse. Those
  sessions get the next-session clamp only and are excluded from jumbo. The ABR decode-cap latch
  likewise does not apply to PyroWave, where adaptive bitrate is open-loop by design.
- **The Deck's Vulkan compatibility layer is built from source**, pinned to the same upstream
  revision as the host's own packaged build — bump both together. ~4 MB of app content replaces a
  94 MB external extension, and Flathub is no longer needed at install time. ⚠ `subprojects/vkroots`
  is a gamescope **submodule** and flatpak-builder clones submodules by default; declaring it again
  as an explicit source breaks the build during extraction. `glm` and `stb` are `.wrap` files, not
  submodules, and *do* need explicit sources.
- **Build-container images push to an authenticated registry endpoint**, and `:latest` is reconciled
  against the content key on every push to main — an out-of-band tag move is detected and repaired
  rather than silently inherited.
- **Windows pad drivers** publish their sequence counters with release ordering (the host was already
  loading with acquire and pairing with nothing) and serialize the output-ring publish. The
  `/dev/uhid` event ABI, previously transcribed into all five Linux gamepad backends, is consolidated
  into one module.

### Verification status

Honest about what has and has not been on hardware, because several things in this release have not:

- **Controller audio has never run on a real DualSense.** Its entire verification is unit tests and
  compile checks, and its rumble arbitration rests on an explicitly retracted assumption about
  whether the voice coils and the rumble motors are the same actuators. The evidence-based 500 ms
  idle window is correct either way, but the underlying exclusivity is unsettled. Android's arbiter
  is the evidence-based one; the desktop twin and the coil restore on Android's stop path are owed.
  Some Android OEM kernels refuse the isochronous claim outright, which degrades to ordinary rumble.
- The **plugin-UI origin split** is validated against a fake console and a fake plugin, not yet in a
  real browser.
- The **packaging default-on changes** have had no installer run or package build.
- **No launcher tile has been clicked on a real host** — the first source that would publish one does
  not exist yet.
- Desktop-audio, packet-sizing and iPad-pointer work is build-verified only.
- ⚠ **The FFmpeg-deletion milestone itself has never executed on a GPU.** It was gated on
  cross-clippy, 160 tests, a workspace check and an `ffmpeg` count of 0 in the client / 2 in the host.
  The software on-glass check, the D3D11 and VAAPI AV1 hardware legs and the field bake were all owed
  at merge; later commits closed some of that but not all. The "no FFmpeg" claim is verified by
  `cargo tree` and a notices-generator mention count, not by inspecting a shipped binary.
- ⚠ **`pf-vaadec` has never decoded a frame anywhere** — no VAAPI hardware was reachable. It is the
  *first* rung on Linux/Intel and unknown vendors; the evidence filter bars it there in favour of
  `pf-vkdecode`, but an explicit pin reaches it.
- **openh264 has never run on glass**; the H.264 software rung is unit-tested only.
- **`native-d3d11va` AV1 is deliberately `verified = false`** — one 25 s 4K60 session, no parity.
- **Split arbitration is opt-in and Linux-wired only**; the Windows arm is built and unit-tested but
  not on hardware. The 5120×1440@240 Main10 behaviour flip is explicitly unvalidated and is named as
  the first thing to re-measure.
- **Software throughput is unmeasured in general** — the CPU rung does 35–39 fps at 4K AV1 against a
  60 fps stream, which is why the backlog flush that triggered the rav1d abort happens at all.
- **The Apple mic fix is a proven root cause, not a verified session.** Its own commits call it "a
  strong inference plus one proven logic defect rather than a confirmed fix" and close "awaiting the
  reporter's on-device confirmation" — which nothing later in the range records. It also leaves a
  known gap: nothing reports whether the uplink actually opened, so the HUD still offers a Mute
  Microphone button over a session that may be sending nothing.
- **The Windows audio substrate is, by contrast, well-evidenced on hardware** — repeated "measured on
  the target box", a live bisect on a fresh endpoint, and a `micpitch` proof reading 440 Hz in →
  440 Hz out at exact peak. The one thing not evidenced is a real client speaking through the minted
  microphone end to end; the pitch proof is probe-driven.
- **The phone-gyro mirror is not recorded as hardware-verified** — remap matrices are pinned by unit
  tests in both languages, but there is no "played a game with a clip-on pad" evidence in the tree.
- **The iOS gamepad-UI pale-palette sweep on glass is still owed**, per its own commit.
- ⚠ **The CI runner scripts are hand-installed** (`/usr/local/bin/ci-docker-prune.sh`,
  `/usr/local/sbin/ci-docker-reclaim.sh`). Merging does not deploy them — both runner hosts need the
  files copied out of `scripts/ci/`, and the missing `192.168.1.58:5011` insecure-registry entry on
  one host is routed around, not fixed.
