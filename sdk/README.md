# @punktfunk/host

TypeScript SDK for the [punktfunk](https://git.unom.io/unom/punktfunk) streaming host: a typed
management-API client plus the host's lifecycle **event stream** (client connect/disconnect,
stream start/stop, pairing, displays, library) — built on [Effect](https://effect.website).

Two surfaces, one core:

- **`@punktfunk/host`** — the Promise facade, the front door. `connect()`, then `pf.api.*` (the
  typed management API — every endpoint autocompletes, every response is typed) and `pf.events.on()`.
  You never need to know Effect exists.
- **`@punktfunk/host/effect`** — the Effect-native surface for plugins and composed programs:
  the `PunktfunkHost` service + layer, `Stream`-based events, typed errors
  (`AuthError | ApiError | TransportError | VersionSkew`), and every wire shape as an
  `effect/Schema` (REST shapes generated from the host's OpenAPI spec; event shapes mirroring
  the host's snapshot-tested wire format).

## Install

Published to the unom [Gitea npm registry](https://git.unom.io/unom/-/packages). Point the
`@punktfunk` scope at it once — in your project's `.npmrc` (or `~/.npmrc`):

```ini
@punktfunk:registry=https://git.unom.io/api/packages/unom/npm/
```

Then install:

```sh
bun add @punktfunk/host      # or: npm i @punktfunk/host
```

`effect` is a **peer dependency** (auto-installed by bun / npm ≥ 7) — so the SDK and your own
`@punktfunk/host/effect` code share one Effect instance.

If the registry requires authentication (private org, or from CI), add a token line with a Gitea
PAT that has `read:package`:

```ini
//git.unom.io/api/packages/unom/npm/:_authToken=${NODE_AUTH_TOKEN}
```

## Quickstart

```ts
import { connect } from "@punktfunk/host";

const pf = await connect(); // zero config on the host box

// Typed API — autocomplete every endpoint, typed responses, no hand-written paths or casts.
const clients = await pf.api.listPairedClients();
console.log(`${clients.length} paired clients`);

// Live events:
pf.events.on("stream.started", (e) => {
  console.log(`${e.stream.client} started ${e.stream.mode}${e.stream.hdr ? " HDR" : ""}`);
});
pf.events.on("pairing.pending", async (e) => {
  // notify your phone, then decide through the typed API:
  const pending = await pf.api.listPendingDevices();
  const match = pending.find((d) => d.fingerprint === e.device.fingerprint);
  if (match) await pf.api.approvePendingDevice(String(match.id), { payload: {} });
});
```

Need something the generated client doesn't cover? `pf.request(method, path, body)` is the untyped
escape hatch (returns `unknown`).

The same, Effect-native:

```ts
import { Effect, Stream } from "effect";
import { events, PunktfunkHostLive } from "@punktfunk/host/effect";

const program = events().pipe(
  Stream.filter((e) => e.kind === "stream.started"),
  Stream.runForEach((e) => Effect.log(`stream: ${e.stream.mode}`)),
);
Effect.runPromise(program.pipe(Effect.provide(PunktfunkHostLive())));
```

## Examples

A complexity ladder in [`examples/`](./examples) — start at the top:

1. [`tail-events.ts`](./examples/tail-events.ts) — **hello world**: connect, one typed call, tail events.
2. [`notify-pairing.ts`](./examples/notify-pairing.ts) — **event → decision**: approve/deny pairing through the typed API.
3. [`provider-sync.ts`](./examples/provider-sync.ts) — **typed bulk REST**: declaratively reconcile a game-library provider.
4. [`couch-preset.effect.ts`](./examples/couch-preset.effect.ts) — **advanced, Effect-native**: only if you're composing Effect programs.

Examples 1–3 are the plain Promise facade and cover most automation; you only need example 4's
Effect surface for composed, interruptible programs. Run any **in the repo** with
`bun examples/<file>.ts`. To **deploy** one on a host, install the package into its own directory
(`bun add @punktfunk/host`) and change its `../src/…` import to `@punktfunk/host` — see
[Running a single script as a service](#running-a-single-script-as-a-service).

Plus a real-world recipe:

- [`virtualhere-dualsense.ts`](./examples/virtualhere-dualsense.ts) — **USB passthrough**: bind a
  real DualSense (shared from the couch over [VirtualHere](https://www.virtualhere.com/)) to the
  host for the length of each connection and release it after — full gyro, touchpad, adaptive
  triggers and USB rumble instead of an emulated pad. Shows the `client.connected`/`disconnected`
  bracket and clean release on `systemctl stop`.

## Connection resolution

`connect()` / `PunktfunkHostLive()` resolve, in order:

| What | Source |
|---|---|
| URL | `{ url }` → `PUNKTFUNK_MGMT_URL` → `<config_dir>/mgmt-endpoint` (the URL the host actually bound, rewritten on every start — a moved `PUNKTFUNK_MGMT_BIND` is followed here) → `https://127.0.0.1:47990` |
| Token | `{ token }` → `PUNKTFUNK_MGMT_TOKEN` → `PUNKTFUNK_PLUGIN_TOKEN` → `<config_dir>/plugin-token` → `<config_dir>/mgmt-token` |
| TLS pin | `{ ca }` → `PUNKTFUNK_MGMT_CA` (path) → `<config_dir>/cert.pem` |

`<config_dir>` is `~/.config/punktfunk` (Linux/macOS) or `%ProgramData%\punktfunk` (Windows) —
so a script running on the host box needs **zero configuration**. The TLS pin trusts exactly
the host's self-signed identity cert (chain-verified; the hostname check is waived — the cert
is deliberately CN-only, native clients pin its fingerprint). Bun and Node are first-class;
other runtimes fall back to system trust (point your runtime's CA option at `cert.pem`).

The zero-config default is the host's **scoped plugin token** (`plugin-token`): the everyday
surface — status, library, sessions, events, the plugin UI lease — but deliberately **not** hook
registration or pairing administration, so a plugin defect can't install commands or admit
devices. A script that needs the full admin surface opts in explicitly with
`PUNKTFUNK_MGMT_TOKEN` or `{ token }` (`mgmt-token` remains the fallback on hosts that predate
the plugin token). Both tokens are honored from loopback only — run scripts on the host box (or
through an SSH tunnel).

## Events

- Reconnects automatically (exponential backoff + jitter, capped) and resumes with
  `Last-Event-ID` — the host replays what you missed from its ring.
- Default is **live tail only** (a fresh notify script must not re-fire on history);
  pass `{ since: 0 }` on the Effect surface to replay the host's full ring, or `since: N`
  to resume after a seq you persisted.
- `on()` patterns: exact kinds (`"stream.started"`, typed callback), `"domain.*"` prefixes,
  `"*"`, plus `"dropped"` (your cursor fell off the ring — resync via REST) and `"unknown"`
  (an event kind newer than this SDK — the additive-only wire at work).
- The host's `live` frame (end of its replay) is swallowed on both surfaces except `eventsRaw()`.
- Effect surface: `events()` is a `Stream<HostEvent, EventStreamError>`; `eventsRaw()` carries
  every SSE frame verbatim.

## Plugins (`punktfunk-plugin-*`)

```ts
import { definePlugin } from "@punktfunk/host";
import { Effect } from "effect";
import { PunktfunkHost } from "@punktfunk/host/effect";

export default definePlugin({
  name: "romm-library",
  main: Effect.gen(function* () {
    const pf = yield* PunktfunkHost;
    // subscribe, sync, reconcile — scoped finalizers run on shutdown/interruption
  }),
  // …or the simple shape: main: async (pf) => { … }
});
```

In v1 a plugin is a script you run (see below); the managed runner package is a later step.

### Persisting state — `pluginStateDir`

A plugin that keeps config or a cache must write it under `pluginStateDir("<your-name>")`, **not**
directly under the config dir:

```ts
import { pluginStateDir } from "@punktfunk/host";
import * as fs from "node:fs";
import * as path from "node:path";

const dir = pluginStateDir("rom-manager"); // <config_dir>/plugin-state/rom-manager
fs.mkdirSync(dir, { recursive: true });
fs.writeFileSync(path.join(dir, "cache.json"), data);
```

This matters on Windows: the managed runner is de-privileged (`NT AUTHORITY\LocalService`) and the
config dir is locked read-only, so a write straight under it fails with `EPERM`. `punktfunk-host
plugins enable` grants the runner write on exactly `plugin-state` — the config dir and your plugin's
*code* stay read-only. On Linux the runner owns the whole config dir, so the same path is writable
with no special step.

### Receiving data from an interactive-user app — `pluginIngestDir`

If a plugin needs data produced by a **different account** — e.g. a desktop app running as the
logged-in user, like the Playnite exporter — it can't read it from that user's profile: the
de-privileged Windows runner can't traverse `C:\Users\<you>\…`. `pluginIngestDir("<your-name>")`
resolves an inbox (`<config_dir>/ingest/<name>`) that `plugins enable` makes **user-writable**, so
your app drops a file there and the runner reads it:

```ts
import { pluginIngestDir } from "@punktfunk/host";
const inbox = pluginIngestDir("playnite"); // <config_dir>/ingest/playnite  (your app writes here)
```

Treat what you read from it as lower trust than your own state — the inbox is writable by any local
user.

### A plugin UI in the console — `servePluginUi`

A plugin can surface a web UI **inside the punktfunk console** — no second password or port for the
operator. It serves the UI on a loopback ephemeral port behind a per-boot secret; `servePluginUi`
registers it with the host, and the console reverse-proxies to it and adds a nav entry gated by the
console's own session. Your code implements **zero human auth**.

```ts
import { definePlugin, servePluginUi } from "@punktfunk/host";

export default definePlugin({
  name: "rom-manager",
  main: async (pf) => {
    const ui = await servePluginUi(pf, {
      id: "rom-manager",
      title: "ROM Manager",
      icon: "gamepad-2",                            // a lucide icon name
      staticDir: new URL("../dist/ui", import.meta.url), // your built SPA
      fetch: (req) => appRouter(req),               // plugin-local REST/SSE (after a static miss)
    });
    try {
      await runForever();
    } finally {
      await ui.close();                             // deregister + stop
    }
  },
});
```

Requests reach `fetch` **prefix-stripped** (the console proxy removed `/plugin-ui/<id>`), so your app
sees `/`, `/api/scan`, … — the original prefix is on `X-Forwarded-Prefix`. `servePluginUi` serves
`staticDir` first (with an `index.html` SPA fallback for navigations); return `undefined` from `fetch`
to fall through to it. Build your SPA with a relative base (`base: "./"` + hash routing) or an absolute
`base: "/plugin-ui/<id>/"`, and expect a dark canvas. Requires the Bun runtime (the runner is bun).

## The runner: `punktfunk-scripting`

Instead of one unit file per script, run everything under the managed runner — it discovers
your units and supervises them:

```sh
bun src/runner-cli.ts            # runs <config_dir>/scripts/* + installed punktfunk-plugin-*
bun src/runner-cli.ts --list     # show what it found
```

The same CLI manages plugin packages — it creates the plugins dir, points it at the `@punktfunk`
registry, and installs on the bun it is already running on:

```sh
bun src/runner-cli.ts add playnite      # → @punktfunk/plugin-playnite (bare names resolve first-party)
bun src/runner-cli.ts remove playnite
bun src/runner-cli.ts list              # installed plugin packages + versions
```

On an installed host these are reached through the host CLI, which also drives the runner service
and checks for elevation on Windows — that is the documented path for operators:

```sh
punktfunk-host plugins add playnite
punktfunk-host plugins enable          # enable + start the runner (opt-in)
punktfunk-host plugins status
```

- **Plugins** (a `definePlugin` default export, from the scripts dir or a
  `punktfunk-plugin-*` package installed under `<config_dir>/plugins/`): supervised — a crash
  restarts them with capped exponential backoff; a clean return completes them.
- **Bare scripts**: importing them is the run — one-shot, no restart (export a plugin to be
  supervised).
- **Shutdown is structural**: SIGINT/SIGTERM interrupt every unit's fiber — Effect plugins'
  scoped finalizers run (release the preset, deregister cleanly) and facade clients close
  before the process exits. This is what makes `systemctl stop` clean.
- The sshd rule applies: a group/world-writable unit file is refused loudly.

systemd user unit for the runner (`~/.config/systemd/user/punktfunk-scripting.service`):

```ini
[Unit]
Description=punktfunk script/plugin runner
After=punktfunk-host.service

[Service]
ExecStart=/usr/bin/bun /path/to/sdk/src/runner-cli.ts
Restart=on-failure
RestartSec=5
# SIGTERM (the default KillSignal) triggers the runner's structured shutdown.

[Install]
WantedBy=default.target
```

## Running a single script as a service

systemd user unit (`~/.config/systemd/user/punktfunk-myscript.service`):

```ini
[Unit]
Description=punktfunk automation: myscript
After=punktfunk-host.service

[Service]
ExecStart=/usr/bin/bun /home/me/punktfunk-scripts/myscript.ts
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Windows Task Scheduler: a task triggered *At log on* running
`bun C:\Users\me\punktfunk-scripts\myscript.ts` (the SDK reads
`%ProgramData%\punktfunk\plugin-token` — run the task as an account that can; the managed
runner's `plugins enable` grants its LocalService principal exactly that read).

## Compatibility

- SDK **majors** track the management-API major; an event `schema` bump or an `effect` major
  is an SDK major too.
- The wire is **additive-only** within a major: an older SDK keeps working against a newer
  host (unknown response keys are ignored; unknown event kinds ride the `"unknown"` channel).
- A 2xx response that doesn't match its schema surfaces as `VersionSkew` on the Effect
  surface — a typed nudge to update, not an `undefined` three frames later.

## Development

```sh
bun install
bun run gen        # regenerate src/gen/punktfunk.ts from ../api/openapi.json (@effect/openapi-generator)
bun run typecheck
bun test
```
