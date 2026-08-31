{
  "_comment": [
    "Punktfunk palette, rendered by Omarchy on every `omarchy-theme-set` from the active theme's",
    "semantic colors.toml into ~/.local/state/omarchy/current/theme/punktfunk.json.",
    "",
    "Installed by `punktfunk-omarchy setup` (optional), removed by `punktfunk-omarchy remove`.",
    "Consumer #1 is the web console, which uses ALL FOUR: mode and accent pick the palette and",
    "re-tint the brand (buttons, nav, focus rings, the lens mark), and every surface — cards,",
    "hovers, borders — is mixed out of the background/foreground pair, so the page belongs to the",
    "theme instead of merely agreeing with its accent. All three colours are required together: a",
    "file missing one is refused outright and the console keeps its own palette.",
    "The host reads nothing from this file — there is no host-side theme engine and no plan for one.",
    "",
    "Consumer #2 is the Linux client (clients/linux/src/omarchy.rs), which redefines libadwaita's",
    "named colours from the same four values. It parses HEX ONLY: it needs the numbers to mix its",
    "surfaces, and GTK's CSS knows nothing of oklch. Colours only — the client keeps its GTK4 shape.",
    "",
    "The webapp window already inherits Omarchy's Chromium theming, so this only has to carry the",
    "colours the page itself paints."
  ],
  "schema": 1,
  "mode": "{{ mode }}",
  "background": "{{ background }}",
  "foreground": "{{ foreground }}",
  "accent": "{{ accent }}"
}
