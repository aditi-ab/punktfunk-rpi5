# punktfunk — an Omarchy shell plugin

Pair devices, watch sessions and control your [Punktfunk](https://punktfunk.com) host from the
Omarchy bar, without opening a browser.

- **bar-widget** — host state and live session count, with a badge when a device is waiting for
  approval. Click opens the panel; right-click opens the web console.
- **panel** — five tabs over a fixed header:
  - *Now* — what is streaming, Stop / End game.
  - *Pairing* — open a window, approve or deny the queue, type a Moonlight PIN.
  - *Devices* — both planes, access level, unpair on hover.
- **service** — one long-lived event stream that drives both of the above and pops an Omarchy toast
  when a device asks to pair.

### Why tabs

The sections stacked in one column outgrew the popup: the ones at the bottom were reachable only by
growing the panel past the screen. Tabs make each subject's height independent, and leave room for
the subjects still to come.

## Install

The host package ships these files at `/usr/share/punktfunk/omarchy/plugin/`, and
[`punktfunk-omarchy setup`](https://punktfunk.com/docs/omarchy) offers to install them into
`~/.config/omarchy/plugins/punktfunk` and enable the widget; `punktfunk-omarchy remove` reverses
both. Requires a Punktfunk host on the same machine — `punktfunk-host` on `PATH`. Omarchy 4.0 or
newer.

## How it talks to the host

Every call is `punktfunk-host ctl <verb> --json`, spawned as a process. **The QML never speaks
HTTPS, never holds the operator token, and never sees the host's certificate.**

That is a deliberate boundary, not an implementation detail. A shell plugin runs *unsandboxed
inside `omarchy-shell`*, and the management API's admin surface — the pending queue, the PIN,
unpair, session control — is exactly the surface worth protecting. So the credential stays where it
already lives: `punktfunk-host ctl` reads the operator token and the host's certificate from the
0700 config directory in its own process, pins the certificate **before** sending the token, and
prints JSON on stdout. If something that is not your host answers on the management port, `ctl`
exits 4 with no credential transmitted, and this plugin shows that state instead of a plausible
"host not running".

`Service.qml`'s `run()` is the only place anything is spawned. Reading it answers "can this plugin
leak a secret?" in about forty lines.

One process runs continuously: `ctl watch`, in `Service.qml`. Exactly one, because the host caps
concurrent event streams and the web console holds one of them. It reconnects by itself and emits a
`ctl.resync` line when it fell behind the host's catch-up ring, which is the plugin's cue to
re-snapshot rather than trust what it has.

## Status

**Loaded and exercised on Omarchy 4.0.2** (Hyprland 0.56.2) on 2026-08-31: `omarchy plugin validate`
passes, the shell loads it with no QML warnings, and every tab renders against a running host.

### What running it caught this time

- **A missing `open: root.opened` on the `KeyboardPanel` failed in total silence.** No QML warning,
  no log line — the panel simply never created its layer surface, while the bar icon kept working.
  If a panel stops opening and nothing is logged, check that binding first.

Three things the first on-glass run caught, all still true:

- **The manifest shape.** Omarchy uses `kinds: [...]` + `entryPoints: { … }`, and the entry-point
  key is camelCase (`barWidget`) while the kind is hyphenated (`bar-widget`). `omarchy plugin
  validate` catches this — run it before you trust an edit.
- **Quickshell's `Process` does not search `PATH`.** It reports "the binary could not be found" for
  a program that is on `PATH` and executable. Every spawn here goes through
  `sh -c 'exec "$@"' sh …`, which does the lookup without re-quoting our argv.
- **`parent.<property>` does not resolve inside `StdioCollector`.** Assign through an explicit `id`
  or the whole call chain silently returns nothing.

### Known limitation: one watcher per monitor

A bar-widget is instantiated once per output, so a two-monitor box runs two `ctl watch` streams
rather than the one this is designed around. The host caps concurrent event streams at 32 and the
web console holds one, so this is comfortably within budget for any realistic desktop — but it is
not what the code says it does, and folding the watcher into a shared singleton is the fix if it
ever matters.

## Reversing it

```sh
omarchy plugin remove punktfunk
```

The plugin owns no state — it stores nothing, and removing it changes nothing about your host, your
pairings or your firewall.

## Licence

MIT OR Apache-2.0, same as Punktfunk.
