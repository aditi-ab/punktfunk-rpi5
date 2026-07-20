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
| `ProviderClient` + wire schemas | typed library-provider reconcile over the untyped wire |
| `makeSyncEngine` | poll + fs-watch + debounce + single-flight coalescing + fingerprint skip + status feed |
| `serveUi` / `httpApiEnv` | an `effect/unstable/httpapi` HttpApi behind the SDK's `servePluginUi`, core-only layers |
| `sseRoute` | the status SSE endpoint (httpapi has no event-stream media type) |
| `runPluginCli` | `<bin> <command>` dispatcher reusing the plugin's layer graph (deliberately not `effect/unstable/cli` — that would drag platform packages into every plugin) |
| `loggingLayer` | runner-journal line format |
| `@punktfunk/plugin-kit/react` | browser glue: `createPluginRouter` (path→hash→fallback deep-link restore + `pf-ui:navigate`), `resolvePluginBase`, `useIsEmbedded`, `ResultGate`, `sseAtom` |
| `@punktfunk/plugin-kit/theme.css` | the console's violet identity for plugin UIs (import first in your Tailwind entry) |

## Publishing

Tag `plugin-kit-vX.Y.Z` (matching `package.json`) — `.gitea/workflows/plugin-kit-publish.yml`
typechecks, tests, builds, and publishes to the Gitea registry.
