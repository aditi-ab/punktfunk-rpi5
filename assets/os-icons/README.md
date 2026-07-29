# OS icon masters

The canonical OS/distro brand marks every client derives its host-card OS icon from
(web console inline SVGs, GTK symbolic icons, Windows PNGs, Apple template imagesets,
Android `ImageVector`s). One file per **icon token** of the host's OS-identity chain
(see `crates/punktfunk-host/src/osinfo.rs` and `crates/pf-client-core/src/os.rs`):

| token | mark | source |
|---|---|---|
| `windows` | Windows | Font Awesome Free brands (CC BY 4.0) |
| `apple` | Apple (also `macos` via alias) | Font Awesome Free brands (CC BY 4.0) |
| `linux` | Tux | Font Awesome Free brands (CC BY 4.0) |
| `steam` | Steam (also `steamos` via alias) | Font Awesome Free brands (CC BY 4.0) |
| `ubuntu` | Ubuntu | Font Awesome Free brands (CC BY 4.0) |
| `fedora` | Fedora | Font Awesome Free brands (CC BY 4.0) |
| `opensuse` | SUSE | Font Awesome Free brands (CC BY 4.0) |
| `arch` | Arch Linux | Simple Icons (CC0 1.0) |
| `nixos` | NixOS | Simple Icons (CC0 1.0) |
| `debian` | Debian | Simple Icons (CC0 1.0) |

Distros with no file here (Bazzite, CachyOS, Nobara, Pop!_OS, Mint, …) are intentional:
the host advertises the full chain (`linux/fedora/bazzite`), and clients walk it
most-specific-first, so they degrade to the family mark and finally to Tux.

All files are monochrome (`fill="currentColor"`), original per-icon viewBoxes preserved.
Licensing: attribution notices live in `LICENSES/` and are folded into
`THIRD-PARTY-NOTICES.txt` by `scripts/gen-third-party-notices.py`. The marks are
trademarks of their respective owners; they are used here nominatively — to *identify*
the operating system a host runs, the standard practice in this ecosystem — and imply no
affiliation or endorsement.
