---
title: Release Channels
description: How punktfunk ships — the canary (every main push) and stable (vX.Y.Z) tracks, how to subscribe to each, and how to cut a release.
---

punktfunk ships on **two tracks**. Every push to `main` publishes a **canary** build to the
canary channels (fast iteration, possibly broken). A `vX.Y.Z` git tag cuts a **stable** release:
every platform is built at that one version, published to the stable channels, and all the
artifacts (`.deb`, `.rpm`, `.msix`, host installer, `.apk`/`.aab`, `.dmg`, flatpak, Decky zip)
are attached to a single [Gitea Release](https://git.unom.io/unom/punktfunk/releases).

The two tracks are **separate repos / tracks per platform**, never a shared version line — so a
stable box never gets pulled onto a canary build, and a canary box always moves forward. Pick the
track per machine; switching is a one-line change.

## Which track should I be on?

- **Canary** — dev boxes, your own test fleet, "I want the latest main build." Updates land minutes
  after a merge.
- **Stable** — anything you don't want to babysit. Only moves when a `vX.Y.Z` tag is cut.

## Subscribe — per platform

| Platform | Canary | Stable |
|---|---|---|
| **apt** (host/client) | `deb [signed-by=…] https://git.unom.io/api/packages/unom/debian canary main` | `… debian stable main` |
| **rpm** (host) | baseurl `…/rpm/bazzite-canary` (or `fedora-44-canary`) | `…/rpm/bazzite` (or `fedora-44`) |
| **Flatpak** (client) | `flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.Canary.flatpakref` | `…/io.unom.Punktfunk.flatpakref` |
| **Decky** (Steam Deck) | install-from-URL `…/generic/punktfunk-decky/canary/punktfunk.zip` | `…/punktfunk-decky/latest/punktfunk.zip` |
| **Windows client** (MSIX) | `…/generic/punktfunk-client-windows/canary/punktfunk-client-windows_x64.msix` | `…/latest/…` + the release page |
| **Windows host** (installer) | `…/generic/punktfunk-host-windows/canary/punktfunk-host-setup.exe` | `…/latest/…` + the release page |
| **Android** | Play **Internal testing** + sideload `…/generic/punktfunk-android/canary/punktfunk-android.apk` | Play **closed (alpha)** track + the release page |
| **Apple** (mac/iOS/tvOS) | **TestFlight** | TestFlight + a notarized `.dmg` on the release page |

The apt distribution and the rpm group are just path segments in the URL — switching tracks is a
one-line edit of `/etc/apt/sources.list.d/punktfunk.list` (`stable` ↔ `canary`) or
`/etc/yum.repos.d/punktfunk.repo` (`…/rpm/bazzite` ↔ `…/rpm/bazzite-canary`), then
`apt update` / `rpm-ostree upgrade`.

> The OS-package channels (apt/rpm) are how Linux hosts get canary builds — they are **not**
> attached to a canary release page. The Gitea Releases page is stable-only.

## Cut a stable release (maintainer)

1. Make sure `main` is green.
2. (Optional) bump any user-facing version that isn't derived from the tag — the Android
   `versionName` fallback (`clients/android/app/build.gradle.kts`) is a cosmetic self-reported
   string; everything else (binaries via `PUNKTFUNK_BUILD_VERSION`, MSIX, apt/rpm, the `.dmg`, and
   the **Decky** plugin version — CI stamps it into `package.json`, where it drives the plugin's own
   [self-update check](/docs/steam-deck#updating)) derives from the tag automatically.
3. Tag and push — **one** tag releases every platform:
   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```
4. Every platform workflow fans out, builds at `0.2.0`, publishes to its **stable** channel, and
   attaches its artifact to the `v0.2.0` Gitea Release. Concurrent attaches are safe — the shared
   `scripts/ci/gitea-release.{sh,ps1}` helper creates the release once and the rest reuse it.
5. **Promote the app stores manually** (CI only uploads to testing tracks — see below).
6. After a release reaches the current canary base, bump the canary base one minor ahead in
   `deb.yml` / `rpm.yml` (and the `0.3.<run>` strings in the other workflows) so a stable→canary
   re-point still moves forward. Rule: **canary base = one minor ahead of the latest stable.**

Pre-release tags work too: `v0.2.0-rc1` builds a real release (the `-rc1` suffix is dropped where a
strictly-numeric version is required — MSIX, the App Store marketing version).

### App-store promotion (manual, after the tag)

CI uploads stable to **testing** tracks only — it never auto-publishes to the public stores:

- **Apple** — the build lands in **TestFlight**. Promote to the App Store from App Store Connect
  (submit for review). The notarized `.dmg` on the release page is the direct-download path.
- **Android** — the build lands in Play's **closed (alpha)** track. Promote alpha → production in
  the Play Console when ready.

## Why two tracks (the version-shadow trap)

apt/rpm/registries serve the **highest** version to every subscriber. If a stable release landed in
the same channel as rolling main builds, every box would jump to it and get **stuck** — the rolling
`0.3.0~ciN` build never climbs above a `0.3.0` release. Separate canary/stable channels remove the
trap by construction, which is why a single `vX.Y.Z` tag can safely release the whole project at
once (the old `host-v*` / `win-v*` / `host-win-v*` tag namespaces are retired — `v*` is the only
release tag now).

## Migrating an existing box to canary

Boxes added before this split point at the current stable channels, which now only move on releases.
Point your dev fleet at **canary**:

```sh
# apt
sudo sed -i 's/ stable main/ canary main/' /etc/apt/sources.list.d/punktfunk.list
sudo apt update && sudo apt upgrade

# rpm-ostree (Bazzite / Fedora)
sudo sed -i 's#/rpm/bazzite#/rpm/bazzite-canary#' /etc/yum.repos.d/punktfunk.repo   # or fedora-44 → fedora-44-canary
rpm-ostree upgrade

# Flatpak (Steam Deck client)
flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.Canary.flatpakref
```
