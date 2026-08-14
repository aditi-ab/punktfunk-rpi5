# Vendored & bundled components — CVE watch and update cadence

Due-diligence record for every third-party component that ships with Punktfunk but is
**not** tracked by a package manager's advisory feed (CRA Art. 13(5); Annex I Part II §1).
Everything resolved through Cargo/bun/pnpm lockfiles is already scanned weekly by
`.gitea/workflows/audit.yml` (cargo-audit against RustSec, bun/pnpm audit) — this file
covers what those scanners cannot see: vendored source trees, git-rev pins, and binaries
staged into installers. The component inventory itself lives in
`compliance/sbom/manual-components.cdx.json` and is merged into every release SBOM;
keep the two files in sync when a component is added, removed, or re-pinned.

Owner for all of it: Enrico (sole maintainer). Standing cadence: **walk this table once
per quarter and before every stable release**; act immediately on any advisory from the
watch feeds below.

| Component | Where / pin | How to update | Watch |
|---|---|---|---|
| **pyrowave** (+ Granite, volk, Vulkan-Headers subtree) | `crates/pyrowave-sys/vendor/pyrowave`, pin = `PYROWAVE_COMMIT` in `scripts/vendor-pyrowave.sh`; exact commits recorded in `vendor/pyrowave/PUNKTFUNK-VENDOR.txt` | Bump the commit in the script, re-run it (network required; never from CI), re-apply `crates/pyrowave-sys/patches/`. ⚠️ **Bitstream changes are protocol-affecting** — the wire bit means "PyroWave as of this pin"; a bitstream-changing bump must bump the protocol version and re-diff the Apple Metal hand-port (see the script header). | GitHub releases/commits of Themaister/pyrowave + Themaister/Granite (niche projects, no CVE feed — repo watch is the feed) |
| **libvpl** 2.17.0 | `crates/libvpl-sys/vendor/libvpl` (dispatcher statically linked; needs cmake + libclang) | Manual re-vendor from intel/libvpl at the new tag; rebuild `libvpl-sys` | Intel Security Center (INTEL-SA advisories for oneVPL/media) + intel/libvpl releases |
| **windows-rs** git pin | `rev = acb5a1a7…` on microsoft/windows-rs (workspace `[patch]`/git deps: `windows`, `windows-reactor`, …) | Move the rev / return to crates.io once the needed fixes are released. Note: cargo-audit matches these by name+version from Cargo.lock, but a pre-release rev may not map cleanly onto RustSec advisories — treat the pin itself as the thing to retire. | RustSec (already weekly) + microsoft/windows-rs releases |
| **usbfs-iso / uac-host** git pin | `rev = f3de1fd…` on unom-io/usbfs-iso | First-party fork — we are upstream; fix in the fork, move the rev | Own repo (issues land in our tracker) |
| **FFmpeg** (host encode only) | Linux: system `libav*` (distro-updated, not ours to patch — but Arch soname majors can break us, see ffmpeg9 note). Windows: AMF/QSV shared DLLs staged from `FFMPEG_DIR` by `pack-host-installer.ps1`; LGPL notice bundled | Windows: rebuild/refresh the staged DLL set, ship in the next installer. Linux: nothing to ship; verify against new distro majors | ffmpeg-security announcements (ffmpeg.org security page) — a libav* CVE in decode/parse paths we use ⇒ refresh the Windows DLLs without undue delay |
| **SDL3** | Desktop clients, dynamically linked; system-provided or bundled per platform package | Bump the bundled copy in the affected package; system copies are distro-updated | libsdl-org/SDL GitHub security advisories + releases |
| **gamescope** + patch series | Pin in `packaging/nix/gamescope.nix` / built by `packaging/gamescope/build-punktfunk-gamescope.sh`; local patches in `packaging/gamescope/patches/` | Bump the pin, re-rebase the patch series, rebuild sysext/Arch/nix + .deb channels. ⚠️ the gamescope CI legs are best-effort: a broken patch shows up as a *missing package*, not a red build | ValveSoftware/gamescope releases + security advisories |
| **Bun runtime** 1.3.14 | Pinned in `.gitea/workflows/windows-host.yml` (`bun-v1.3.14`); bundled portable in the Windows host installer to run the web console + plugin runner. Embeds JavaScriptCore | Bump the version string in the workflow; next installer build picks it up | oven-sh/bun releases (security notes ride in release notes) |

Not on this list on purpose:

- **VB-CABLE** — no longer bundled (audio-substrate program, 2026-08; the host mints its
  own virtual audio devices). If it ever returns, it returns to this table first.
- **openh264 / rav1d CPU decode floor** — crates.io dependencies with vendored C/asm
  inside the `-sys` crates; cargo-audit tracks the crate advisories, and the upstream
  (Cisco openh264, memorysafety/rav1d) security feeds surface through RustSec. No
  separate manual watch needed unless we pin them to git.

## Security-update availability (CRA: ≥10 years)

Where users fetch fixes, and why old artifacts don't vanish (verified 2026-08-14):

- **Gitea releases + package registries** (git.unom.io): no cleanup rules configured,
  and Gitea does not expire releases or packages on its own — the full release history
  (v0.17.x through current) is still served with assets. Blobs live in the `unom-git`
  S3 bucket with an R2 mirror, and the box is restic-backed every 6 h. Old release
  assets (and their `.sha256` sidecars) therefore stay downloadable.
- **Bazzite sysext feeds**: stable channels publish with `KEEP=0` (keep everything);
  only canary channels prune (`KEEP=6`) — see `rpm.yml` + `publish-sysext-feed.sh`.
- **Flatpak repo** (flatpak.unom.io): published by rsync *without* `--delete`; old
  OSTree commits accumulate, both channels stay in the signed summary.
- **Policy**: never add cleanup that deletes *security* releases; if storage pressure
  ever forces pruning, prune canary builds, never tagged stable releases. SBOMs are
  release assets, so the ≥10-year SBOM retention rides on the same guarantee.
