---
title: "CI & Docker"
description: "Gitea Actions setup — workflows, the dockerized pieces, and the runners."
---

CI runs on **Gitea Actions** (`git.unom.io`, org `unom`). The workflows live in
`.gitea/workflows/`; they run across Linux and macOS runners and push a few images to the
Gitea container registry.

## Workflows

| Workflow | Trigger | Runner | What it does |
|---|---|---|---|
| `ci.yml` | push to `main`, PRs | Linux | Rust workspace (fmt · clippy `-D warnings` · build · test · C-ABI harness · generated-header drift) inside the `punktfunk-rust-ci` image; `web/` and `docs-site/` build + typecheck in `oven/bun:1` |
| `docker.yml` | push to `main`, `v*` tags, manual | Linux | Builds + pushes the images below (`latest` + `sha-<short>` tags) |
| `apple.yml` | push to `main`, PRs, manual | macOS | Rust core → `PunktfunkCore.xcframework` → `swift build` + `swift test` in `clients/apple` |
| `release.yml` | `v*` tags, manual | macOS | Production Apple builds: sandboxed macOS `.dmg` (Developer ID, notarized, stapled) attached to the Gitea release + macOS/iOS/tvOS archives uploaded to TestFlight |
| `windows-msix.yml` | push to `main`, `v*` tags, manual | Windows | Builds the Windows client for `x86_64`/`aarch64` and packages signed MSIX artifacts |

## Dockerized pieces

The host and the native clients are intentionally **not** containerized (the host needs
the GPU/compositor stack of the box it runs on). What is:

| Image | Source | Notes |
|---|---|---|
| `git.unom.io/unom/punktfunk-web` | `web/Dockerfile` (repo-root context — orval needs `docs/api/openapi.json`) | Nitro `bun` bundle; `PORT` (3000) and `PUNKTFUNK_MGMT_URL` env at runtime |
| `git.unom.io/unom/punktfunk-docs` | `docs-site/Dockerfile` | This site; `PORT` (3000) |
| `git.unom.io/unom/punktfunk-rust-ci` | `ci/rust-ci.Dockerfile` | Ubuntu 26.04 + FFmpeg 8/PipeWire/GL/GBM dev libs + a libcuda **link stub** (driver userspace, no kernel module) + pinned rustup — the container `ci.yml`'s Rust job runs in |

Registry pushes authenticate with a repo Actions secret holding a registry token (a PAT
with `write:package`; the login username in `docker.yml` is the token owner, not the
push actor).

## Runners

- **Linux runner** — runs the Rust/web/docs jobs (as docker containers) and the image
  build+push jobs.
- **macOS runner** — an Apple-silicon Mac running macOS, a **host-mode** `act_runner`
  (upstream now ships it as `gitea-runner`) provisioned by
  [`scripts/ci/setup-macos-runner.sh`](https://git.unom.io/unom/punktfunk/src/branch/main/scripts/ci/setup-macos-runner.sh):
  rustup (+ both darwin targets for the universal xcframework), Node.js (host-mode runners
  execute JS actions via `node` from PATH — nothing auto-provisions it), the runner binary
  in `~/.local/bin`, state under `~/ci/act-runner/` (config, `.runner` registration,
  `runner.log`), kept alive by the `io.gitea.act_runner` **root LaunchDaemon** — it cannot
  be a user LaunchAgent: macOS Local Network privacy silently blocks LAN dials
  ("no route to host") from unbundled CLI binaries in gui/user launchd domains, while
  system daemons are exempt. Needs full **Xcode** for `xcodebuild -create-xcframework`
  (CLT alone only covers `swift build/test`); if `xcode-select` still points at CLT, the
  script auto-detects `/Applications/Xcode*.app` and bakes a `DEVELOPER_DIR` override into
  the daemon environment — no `xcode-select -s` required.
- **Windows runner** — builds and packages the native Windows client (MSIX) for the
  release matrix.

Re-provisioning is idempotent — re-running `scripts/ci/setup-macos-runner.sh` on the macOS
runner with a fresh `GITEA_RUNNER_TOKEN` (org `unom` → Settings → Actions → Runners →
Create new runner) re-registers it without manual cleanup.

## Apple releases

`release.yml` produces the production client builds on the Mac runner. All three app
targets share the bundle ID **`io.unom.punktfunk`** (one App Store listing, universal
purchase — effectively unchangeable after first submission). Signing is **not** secret-based:
the runner uses its **login keychain** directly, so install the **Developer ID Application**,
**Apple Distribution**, and (for the Mac App Store `.pkg`) **3rd Party Mac Developer
Installer** identities once via Xcode, with the WWDR intermediate present so they show as
valid. The only secrets are `ASC_API_KEY_P8`/`ASC_API_KEY_ID`/`ASC_API_ISSUER_ID` (App Store
Connect API key — notarization + TestFlight upload). Per-platform state:

- **macOS (Developer ID)** — sandboxed app (`Config/Punktfunk-macOS.entitlements`) → export
  → `notarytool` → stapled `.dmg` on the Gitea release.
- **macOS (App Store)** — manual-signed archive (Apple Distribution + the *Punktfunk macOS
  App Store Distribution* profile) → upload to TestFlight. App Sandbox is **mandatory** here
  and is now declared (app-sandbox + network client/server + audio-input + bluetooth/usb).
  Prereqs (one-time, Apple portal): add the **macOS platform** to the App Store Connect app
  record (universal purchase), install the Mac App Store distribution profile + the installer
  cert above. `continue-on-error` until those exist.
- **iOS** — archive + upload to TestFlight (`method: app-store-connect`,
  `destination: upload`). Crypto is declared exempt (`ITSAppUsesNonExemptEncryption`,
  `Config/Info.plist`) so builds don't stall on the compliance question.
- **tvOS** — archive + upload to TestFlight (Rust core built from tier-3 targets, nightly
  `-Zbuild-std` via `build-xcframework.sh`).

Each macOS target uses its own entitlements: `Config/Punktfunk-macOS.entitlements` (App
Sandbox is macOS-only) for the macOS app, and the shared `Config/Punktfunk.entitlements`
(keychain-access-groups only) for iOS/tvOS — `com.apple.security.app-sandbox` is invalid on
iOS/tvOS and would fail upload validation.

The runner needs a **release (non-beta) Xcode** — App Store processing rejects beta-SDK
builds, and a beta is unusable for the Rust side too: a newer-than-OS ld emits dylibs the
running dyld rejects ("mis-aligned LINKEDIT string pool"), killing every proc-macro build
with a misleading `E0463 can't find crate`. `build-xcframework.sh` therefore resolves
toolchains itself: non-beta Xcode for everything; with only CLT + a beta present it
builds macOS slices against CLT (packaging via any Xcode — `-create-xcframework` does no
linking) and **refuses iOS/tvOS slices** (CLT has no iOS SDK).

## Deployment

`docker.yml`'s `deploy-docs` job ships this docs site after every image push: it syncs
`compose.production.yml` to the docs server and runs `docker compose pull && up -d` there
over SSH, driven by a small set of deploy secrets (`DEPLOY_HOST` / `DEPLOY_USER` /
`DEPLOY_PORT` / `DEPLOY_SSH_KEY`). A reverse proxy in front of that server serves the
container as <https://docs.punktfunk.unom.io>. The host and the web console are NOT
deployed — the console fronts a punktfunk host's management API on whatever box runs the
host.

## Troubleshooting

- **macOS runner offline** — check `~/ci/act-runner/runner.log` on the runner; restart with
  `sudo launchctl kickstart -k system/io.gitea.act_runner`. "no route to host" in the log
  means the daemon is running in a gui/user domain again — see the Local Network note
  above.
- **`apple.yml` fails at the xcframework step** — Xcode missing or unselected:
  `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer` and accept the license
  (`sudo xcodebuild -license accept`), then re-run.
- **Rust job can't pull `punktfunk-rust-ci`** — the runner host's docker daemon needs a
  `docker login git.unom.io` if the org/registry isn't anonymously readable.
- **Stale builder image after toolchain/dep changes** — `docker.yml` re-pushes it on every
  `main` push; a manual `workflow_dispatch` of `docker.yml` forces a rebuild.
