# Lucide icon masters

The canonical UI marks every client draws its own icons from — the quick-action ring's slots,
and the ordinary shell chrome (back, refresh, save, delete…) on the desktop clients.

[Lucide](https://lucide.dev) **v0.462.0**, ISC licensed (see `THIRD-PARTY-NOTICES.txt`), fetched
unmodified from `lucide-icons/lucide` at that tag. One file per icon, its own name. Every master
is a 24×24 `viewBox`, `fill="none"`, `stroke="currentColor"`, `stroke-width="2"`, round caps and
joins — Lucide's own drawing contract, and what every derivative below reproduces.

## Which client consumes what

`scripts/gen-lucide-assets.sh` derives all of it. Nothing here is hand-edited.

| client | form | where |
|---|---|---|
| Skia console (gamepad UI) | folded path string, stroked by Skia | `crates/pf-client-core/src/lucide.rs` → `crates/pf-console-ui/src/icons.rs` |
| GTK shell | the same path string, stroked by `gsk::Path` | `crates/pf-client-core/src/lucide.rs` |
| WinUI shell | PNG, baked twice (grey and white) | `clients/windows/assets/lucide/` |

The two Rust consumers share **one** table, so a mark cannot differ between the console and the
GTK shell. The WinUI shell bakes because windows-reactor has no vector element: its `Image` takes
a raster URI, and its `BitmapIcon` is created with `ShowAsMonochrome(false)`, so a WinUI icon
cannot be tinted at runtime and has to ship in the colour it will be drawn in. Hence two bakes —
`lucide/` in mid-grey for ordinary surfaces, `lucide-on/` in white for accent buttons and the
ring's dark discs.

## Adding an icon

1. Drop the master here: `curl -o assets/lucide/<name>.svg
   https://raw.githubusercontent.com/lucide-icons/lucide/0.462.0/icons/<name>.svg`
2. `bash scripts/gen-lucide-assets.sh`
3. Add the PNG to `clients/windows/src/app/lucide.rs`'s `ICONS` table — the shipped-token list,
   the same discipline the OS marks and launcher marks keep.

The console's `icons.rs` and the GTK shell need no list of their own: both read
`pf_client_core::lucide`, which the script regenerates whole.
