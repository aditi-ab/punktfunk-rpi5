# Changelog

Protocol, ABI, driver and embedder detail, one section per stable release, newest first.

This is the **technical** half of a release. The other half — what changed for people who *use*
Punktfunk — is `docs/releases/vX.Y.Z.md`, and it deliberately contains no internal names. The two
were one document through v0.24.0; they split at v0.25.0 because the engineering section had grown
long enough to bury the user-facing half it was appended to. See `docs/releases/README.md`.

If you embed `punktfunk-core`, package Punktfunk, or write a plugin, this file is for you. Start
with the version table of the release you are moving to, then read **Breaking changes**.

---

## v0.28.1 — in development

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
