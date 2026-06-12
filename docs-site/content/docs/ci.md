---
title: "CI & Docker"
description: "Gitea Actions setup — workflows, the dockerized pieces, and the runners."
---

CI runs on **Gitea Actions** (`git.unom.io`, org `unom`). Three workflows in
`.gitea/workflows/`, two runners, three images in the Gitea container registry.

## Workflows

| Workflow | Trigger | Runner | What it does |
|---|---|---|---|
| `ci.yml` | push to `main`, PRs | `ubuntu-24.04` | Rust workspace (fmt · clippy `-D warnings` · build · test · C-ABI harness · generated-header drift) inside the `punktfunk-rust-ci` image; `web/` and `docs-site/` build + typecheck in `oven/bun:1` |
| `docker.yml` | push to `main`, `v*` tags, manual | `ubuntu-24.04` | Builds + pushes the three images below (`latest` + `sha-<short>` tags) |
| `apple.yml` | push to `main`, PRs, manual | `macos-arm64` | Rust core → `PunktfunkCore.xcframework` → `swift build` + `swift test` in `clients/apple` |

## Dockerized pieces

The host and the native clients are intentionally **not** containerized (the host needs
the GPU/compositor stack of the box it runs on). What is:

| Image | Source | Notes |
|---|---|---|
| `git.unom.io/unom/punktfunk-web` | `web/Dockerfile` (repo-root context — orval needs `docs/api/openapi.json`) | Nitro `bun` bundle; `PORT` (3000) and `PUNKTFUNK_MGMT_URL` env at runtime |
| `git.unom.io/unom/punktfunk-docs` | `docs-site/Dockerfile` | This site; `PORT` (3000) |
| `git.unom.io/unom/punktfunk-rust-ci` | `ci/rust-ci.Dockerfile` | Ubuntu 26.04 + FFmpeg 8/PipeWire/GL/GBM dev libs + a libcuda **link stub** (driver userspace, no kernel module) + pinned rustup — the container `ci.yml`'s Rust job runs in |

Registry pushes authenticate with the repo Actions secret **`REGISTRY_TOKEN`** (a PAT
with `write:package`; the login username in `docker.yml` is the token owner, not the
push actor).

## Runners

- **`ubuntu-24.04`** — the pre-existing Linux runner; runs the Rust/web/docs jobs (as
  docker containers) and the image build+push jobs.
- **`macos-arm64`** — `home-mac-mini-1` (M-series, macOS 26), a **host-mode**
  `act_runner` (upstream now ships it as `gitea-runner`) provisioned by
  [`scripts/ci/setup-macos-runner.sh`](https://git.unom.io/unom/punktfunk/src/branch/main/scripts/ci/setup-macos-runner.sh):
  rustup (+ both darwin targets for the universal xcframework), Node.js (host-mode runners
  execute JS actions via `node` from PATH — nothing auto-provisions it), the runner binary
  in `~/.local/bin`, state in `~/ci/act-runner/` (config, `.runner` registration,
  `runner.log`), kept alive by the `io.gitea.act_runner` **root LaunchDaemon** — it cannot
  be a user LaunchAgent: macOS Local Network privacy silently blocks LAN dials
  ("no route to host") from unbundled CLI binaries in gui/user launchd domains, while
  system daemons are exempt. Needs full **Xcode** for `xcodebuild -create-xcframework`
  (CLT alone only covers `swift build/test`); if `xcode-select` still points at CLT, the
  script auto-detects `/Applications/Xcode*.app` and bakes a `DEVELOPER_DIR` override into
  the daemon environment — no `xcode-select -s` required.

Re-provisioning (idempotent) or first-time registration from a dev box:

```sh
# token: org unom → Settings → Actions → Runners → Create new runner
ssh enricobuehler@192.168.1.135 GITEA_RUNNER_TOKEN=<token> bash -s \
    < scripts/ci/setup-macos-runner.sh
```

## Deployment

`docker.yml`'s `deploy-docs` job ships this docs site after every image push: it syncs
`compose.production.yml` to `~/punktfunk-docs` on **unom-1** (the DMZ services VM
website and cms deploy to) and runs `docker compose pull && up -d` there over SSH (same
pattern and secret set as `unom/website`: `DEPLOY_HOST` / `DEPLOY_USER` / `DEPLOY_PORT` /
`DEPLOY_SSH_KEY`, the `unom-ci-deploy` key). The container binds host port **3220**;
Caddy on `home-reverse-proxy-1` serves it as <https://docs.punktfunk.unom.io> (vhost in
`unom/reverse-proxy`, UniFi firewall allowlist Caddy→unom-1:3220 in `unom/infra`
`proxmox/unom-1`). The host and the web console are NOT deployed — the console
fronts a punktfunk host's management API on whatever box runs the host.

## Troubleshooting

- **Mac runner offline** — `ssh <mac> tail -50 '~/ci/act-runner/runner.log'`; restart with
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
