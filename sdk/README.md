# @punktfunk/host

TypeScript SDK for the [punktfunk](https://git.unom.io/unom/punktfunk) streaming host: a typed
management-API client plus the host's lifecycle **event stream** (client connect/disconnect,
stream start/stop, pairing, displays, library) — built on [Effect](https://effect.website).

Two surfaces, one core:

- **`@punktfunk/host`** — the Promise facade, the front door. `connect()`, `await`, `.on()`.
  You never need to know Effect exists.
- **`@punktfunk/host/effect`** — the Effect-native surface for plugins and composed programs:
  the `PunktfunkHost` service + layer, `Stream`-based events, typed errors
  (`AuthError | ApiError | TransportError | VersionSkew`), and every wire shape as an
  `effect/Schema` (REST shapes generated from the host's OpenAPI spec; event shapes mirroring
  the host's snapshot-tested wire format).

## Quickstart

```ts
import { connect } from "@punktfunk/host";

const pf = await connect(); // zero config on the host box

pf.events.on("stream.started", (e) => {
  console.log(`${e.stream.client} started ${e.stream.mode}${e.stream.hdr ? " HDR" : ""}`);
});
pf.events.on("pairing.pending", async (e) => {
  // notify your phone, then decide through the API:
  // await pf.request("POST", `/native/pending/${id}/approve`);
});
```

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

## Connection resolution

`connect()` / `PunktfunkHostLive()` resolve, in order:

| What | Source |
|---|---|
| URL | `{ url }` → `PUNKTFUNK_MGMT_URL` → `https://127.0.0.1:47990` |
| Token | `{ token }` → `PUNKTFUNK_MGMT_TOKEN` → `<config_dir>/mgmt-token` |
| TLS pin | `{ ca }` → `PUNKTFUNK_MGMT_CA` (path) → `<config_dir>/cert.pem` |

`<config_dir>` is `~/.config/punktfunk` (Linux/macOS) or `%ProgramData%\punktfunk` (Windows) —
so a script running on the host box needs **zero configuration**. The TLS pin trusts exactly
the host's self-signed identity cert (chain-verified; the hostname check is waived — the cert
is deliberately CN-only, native clients pin its fingerprint). Bun and Node are first-class;
other runtimes fall back to system trust (point your runtime's CA option at `cert.pem`).

The bearer token is the host's **admin** credential and is honored from loopback only — run
scripts on the host box (or through an SSH tunnel).

## Events

- Reconnects automatically (exponential backoff + jitter, capped) and resumes with
  `Last-Event-ID` — the host replays what you missed from its ring.
- Default is **live tail only** (a fresh notify script must not re-fire on history);
  pass `{ since: 0 }` on the Effect surface to replay the host's full ring, or `since: N`
  to resume after a seq you persisted.
- `on()` patterns: exact kinds (`"stream.started"`, typed callback), `"domain.*"` prefixes,
  `"*"`, plus `"dropped"` (your cursor fell off the ring — resync via REST) and `"unknown"`
  (an event kind newer than this SDK — the additive-only wire at work).
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

## The runner: `punktfunk-scripting`

Instead of one unit file per script, run everything under the managed runner — it discovers
your units and supervises them:

```sh
bun src/runner-cli.ts            # runs <config_dir>/scripts/* + installed punktfunk-plugin-*
bun src/runner-cli.ts --list     # show what it found
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
`%ProgramData%\punktfunk\mgmt-token` — run the task as an account that can).

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
bun run gen        # regenerate src/gen/schemas.ts from ../api/openapi.json
bun run typecheck
bun test
```
