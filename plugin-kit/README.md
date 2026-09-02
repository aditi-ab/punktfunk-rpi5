# @punktfunk/plugin-kit

The Effect-based framework punktfunk plugins are built on. It owns everything that is the
same in every plugin — lifecycle, config/state, the sync engine, UI serving, the CLI
scaffold, logging — so a plugin is just its domain logic, its HttpApi contract, and its UI.
The reference consumer (and the blueprint to copy) is
[`punktfunk-plugin-rom-manager`](https://git.unom.io/unom/punktfunk-plugin-rom-manager).

Built on [`@punktfunk/host`](../sdk) (the SDK stays the low-level host client; the kit is
the opinionated plugin layer on top). Effect `4.x` and the SDK are peer dependencies —
the plugin's own copies are the only copies.

## The one rule: async at the boundary, Effect inside

The packaged runner bundles its own effect + SDK; a plugin's imports resolve to the
plugin's node_modules. Effect values must therefore never cross the plugin boundary
(`Context.Tag` identity is per-instance). `definePluginKit` enforces this by construction:
you write Effect, it exports a plain async-`main` `PluginDef`, and a `ManagedRuntime`
built from *your* effect instance runs everything. SIGINT/SIGTERM interrupt the plugin
fiber (scoped finalizers run: UI deregistration, watcher close), bounded by
`shutdownGraceMs`.

```ts
import { definePluginKit, serveUi } from "@punktfunk/plugin-kit";
import { Effect, Layer } from "effect";

export default definePluginKit({
  name: "my-plugin",
  version: "0.1.0",
  layer: MyServices.layer, // over the kit base: HostClient | PluginInfo
  main: Effect.gen(function* () {
    const engine = yield* MySync;
    yield* engine.start;
    yield* serveUi({ title: "My Plugin", icon: "puzzle", staticDir, api: MyApiLive });
    yield* Effect.never;
  }),
});
```

## Modules

| Export | What it owns |
| --- | --- |
| `definePluginKit` / `runPluginKitDirect` | the async-main boundary + ManagedRuntime + signal handling |
| `HostClient`, `PluginInfo` | the `pf` facade as services (`request` = the skew-safe untyped seam) |
| `makeConfigService` | Schema-driven config: raw shape on disk, defaults ONLY in the Schema (`withDecodingDefaultKey` + `encodingStrategy: "omit"`), atomic writes, world-writable refusal, `changes` stream |
| `makeCacheStore` | disposable derived state (corrupt/absent → empty, write-through) |
| `ProviderClient` + wire schemas | typed library-provider reconcile over the untyped wire — including the optional `detect` hint (see below) |
| `makeSyncEngine` | poll + fs-watch + debounce + single-flight coalescing + fingerprint skip (loop triggers only — `startup` and `manual` always publish) + status feed |
| `serveUi` / `httpApiEnv` | an `effect/unstable/httpapi` HttpApi behind the SDK's `servePluginUi`, core-only layers |
| `sseRoute` | the status SSE endpoint (httpapi has no event-stream media type) |
| `runPluginCli` | `<bin> <command>` dispatcher reusing the plugin's layer graph (deliberately not `effect/unstable/cli` — that would drag platform packages into every plugin) |
| `loggingLayer` | runner-journal line format |
| `@punktfunk/plugin-kit/react` | browser glue: `createPluginRouter` (path→hash→fallback deep-link restore + `pf-ui:navigate`), `resolvePluginBase`, `useIsEmbedded`, `ResultGate`, `sseAtom` |
| `@punktfunk/plugin-kit/theme.css` | the console's violet identity for plugin UIs (import first in your Tailwind entry) |
| `@punktfunk/plugin-kit/library` | everything a **game-library scanner** plugin needs — see below |

## Library-scanner plugins (`@punktfunk/plugin-kit/library`)

The six first-party scanners (steam, lutris, heroic, epic, gog, xbox) each live in **their own
repo**, like every other punktfunk plugin. Nothing is lost by that split because everything they
share is published here rather than sitting adjacent to them:

| Export | What it saves you writing |
| --- | --- |
| `defineLibraryPlugin` | the whole plugin except the scan: store claim, sync engine (poll + fs-watch + debounce), launcher entries, `__config`, `category: "library"` registration, the `detect` / `scan` / `parity` / `uninstall` CLI verbs, and the `__launch` answer for `kind: "plugin"` tiles (pass `launch(cfg, entry)`) |
| `parsers/*` | text VDF + `.acf`, binary `shortcuts.vdf` (with the CRC-32 appid and the 64-bit `rungameid` composition), read-only SQLite, `reg.exe`, capped readers, a confined path join, Steam root/library discovery, art location helpers, an anti-SSRF fetch |
| `diffParity` + the `parity` verb | the acceptance gate below |

A first-party scanner is therefore **its parsers and a `scan` function** — a few hundred lines.

### The parity gate

Ported unit tests pin the parsers; they do not prove the plugin reproduces the scanner it replaces.
A plugin that parses perfectly and emits `steam:440.0` instead of `steam:440` breaks every Moonlight
pin on the host, and no parser test notices. So, on a box with that launcher installed:

```sh
# 1. while the host is still using its BUILT-IN scanner:
punktfunk-plugin-steam parity --snapshot before.json
# 2. offline — runs this plugin's own scan and diffs:
punktfunk-plugin-steam parity --compare before.json
```

`--compare` exits non-zero on any difference, so it works as a release gate. It compares ids,
titles, launch recipes, roles and metadata exactly; **art by presence, not value** (the
representation legitimately changes — a host-relative proxy path or inlined `data:` URL becomes a
`file://` path or a CDN URL), so spot-check a few covers by eye once. Launcher entries the plugin
adds are reported separately rather than failing the run; an ordinary title the scanner never had
still fails.

## Telling the host how to recognize a running title (`detect`)

A `ProviderEntry` may carry an optional `detect` hint:

```ts
{ external_id: "playnite:9f2…", title: "Hades",
  launch: { kind: "command", value: "playnite://playnite/start/9f2…" },
  detect: { install_dir: "D:\\Games\\Hades" } }
```

It is what lets the host tell that the *game* has exited — which ends the streaming session, so the
player's client returns to its library instead of showing a bare desktop — and what lets an operator
who opted into it end the game when the session ends.

Omit it and nothing breaks: the host tracks the process it spawns for your launch command. It matters
when that command **hands off and exits** — a launcher client, `flatpak run`, a front-end that starts
an emulator — because then there is nothing left for the host to watch, and both behaviors go quiet
for that title. Send whatever you genuinely know; `install_dir` is the one to send if you send only
one, since any process running from under it counts as the game. The host never lets a hint override
what it worked out itself, and never adopts a process that was already running before the launch.

## Publishing

Tag `plugin-kit-vX.Y.Z` (matching `package.json`) — `.gitea/workflows/plugin-kit-publish.yml`
typechecks, tests, builds, and publishes to the Gitea registry.
