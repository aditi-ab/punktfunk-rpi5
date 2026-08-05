// `defineLibraryPlugin` — the shared framework behind every library-scanner plugin (design D10).
//
// The point of this module is that a first-party scanner should be **its parsers and a scan
// function**, ~200–400 lines, and nothing else. Everything a scanner needs beyond that is identical
// across all six of them and lives here: claiming the store, reconciling through the sync engine,
// appending launcher entries, serving `__config` so the console renders settings without the plugin
// shipping an SPA, registering under `category: "library"` so it stays out of the nav, and the
// standard CLI verbs.
import type { PluginDef } from "@punktfunk/host";
import { Duration, Effect, Layer, Schema, Stream } from "effect";
import { type CliCommand, runPluginCli } from "../cli.js";
import { type ConfigService, makeConfigService } from "../config.js";
import { type HostClient, PluginInfo } from "../host-client.js";
import { ProviderClient, type ProviderClientService } from "../reconcile.js";
import { definePluginKit, type PluginKitDef } from "../runtime.js";
import { makeSyncEngine } from "../sync-engine.js";
import { serveUi } from "../ui-server.js";
import type { ProviderEntry } from "../wire.js";

/** What a scan produced — the status surface and the CLI's `scan` verb both render this. */
export interface ScanReport {
	readonly entries: number;
	readonly launchers: number;
	/** False when the launcher isn't installed here — the library is legitimately empty. */
	readonly present: boolean;
}

export interface LibraryPluginDef<S extends Schema.Top> {
	/**
	 * The plugin id. **This one string is also the provider id, the store claim, and the id of the
	 * built-in scanner this plugin replaces.** That identity chain is what makes the migration
	 * invisible: entry ids stay `<name>:<external_id>`, GameStream app ids and client art caches
	 * stay valid, and the operator's existing enable/disable state carries over untouched.
	 */
	readonly name: string;
	readonly version?: string;
	/**
	 * The store to claim (design D2). Defaults to {@link name} and should almost never differ — see
	 * the identity note above. Pass `null` to opt out of claiming entirely, which makes this an
	 * ordinary unclaimed provider whose entries surface as `custom:`.
	 */
	readonly store?: string | null;
	/** The operator-facing config schema. Drives `__config` and every callback's argument. */
	readonly configSchema: S;
	/**
	 * Is this launcher present on the host at all? Surfaces in the CLI's `detect` verb, and lets the
	 * plugin report "not installed" rather than silently syncing an empty library.
	 */
	readonly detect: (cfg: S["Type"]) => Effect.Effect<boolean>;
	/** Enumerate the launcher's installed titles — the only real per-store code. */
	readonly scan: (
		cfg: S["Type"],
	) => Effect.Effect<ReadonlyArray<ProviderEntry>>;
	/**
	 * Entries that open the LAUNCHER itself (design D4) — Steam Big Picture, Heroic, … Appended to
	 * every reconcile, so toggling one in config takes effect on the next sync. Emit them with
	 * `role: "launcher"`; the kit does not stamp it for you, because a plugin may legitimately want
	 * an entry that opens a launcher but still lists as an ordinary game.
	 */
	readonly launchers?: (cfg: S["Type"]) => ReadonlyArray<ProviderEntry>;
	/** Launcher data dirs to watch, so a newly installed game appears without waiting for a poll. */
	readonly watchDirs?: (cfg: S["Type"]) => ReadonlyArray<string>;
	/** How often to re-scan regardless of watches. Default `Duration.minutes(15)`. */
	readonly pollInterval?: Duration.Duration;
	/** Debounce on filesystem events. Default `Duration.seconds(3)`. */
	readonly debounce?: Duration.Duration;
	/** Display title (the console's sources row falls back to the scanner label). Defaults to `name`. */
	readonly title?: string;
	/** Extra CLI verbs beyond the standard `detect` / `scan` / `uninstall` set. */
	readonly commands?: Record<string, CliCommand<never>>;
}

/** The pieces a library plugin package wires into its entry points. */
export interface LibraryPlugin {
	/** The runner-discovered default export (`export default plugin.def`). */
	readonly def: PluginDef;
	/** The CLI entry (`await plugin.cli()` from the package's bin). */
	readonly cli: (argv?: ReadonlyArray<string>) => Promise<void>;
}

export const defineLibraryPlugin = <S extends Schema.Top>(
	def: LibraryPluginDef<S>,
): LibraryPlugin => {
	const store = def.store === null ? undefined : (def.store ?? def.name);
	const poll = def.pollInterval ?? Duration.minutes(15);
	const debounce = def.debounce ?? Duration.seconds(3);

	/** The config service, built fresh wherever it is needed (it only requires `PluginInfo`). */
	const config: Effect.Effect<ConfigService<S>, never, PluginInfo> =
		makeConfigService({ schema: def.configSchema });

	/** Scan + launcher entries, in the order they should reach the host. */
	const computeEntries = (
		cfg: S["Type"],
	): Effect.Effect<{
		readonly entries: ReadonlyArray<ProviderEntry>;
		readonly report: ScanReport;
	}> =>
		Effect.gen(function* () {
			const present = yield* def.detect(cfg);
			// A launcher that isn't installed contributes NOTHING — not even its launcher entries. A
			// "Steam Big Picture" tile on a box without Steam would only fail to launch.
			if (!present) {
				return {
					entries: [] as ReadonlyArray<ProviderEntry>,
					report: { entries: 0, launchers: 0, present: false } as const,
				};
			}
			const scanned = yield* def.scan(cfg);
			const launchers = def.launchers?.(cfg) ?? [];
			return {
				entries: [...scanned, ...launchers],
				report: {
					entries: scanned.length,
					launchers: launchers.length,
					present: true,
				} as const,
			};
		});

	/**
	 * Push one entry set to the host under the store claim, warning **once** if the host is too old
	 * to honour it.
	 *
	 * This degradation is worth the code: a pre-M2 host ignores `?store=` silently, and the only
	 * symptom would be this plugin's titles appearing as unbadged `custom:` entries *beside* the
	 * built-in scanner's identical ones — a confusing double-listing with no error anywhere.
	 * Checking the echoed entries turns that into one actionable log line.
	 */
	const applyEntries =
		(provider: ProviderClientService, state: { warned: boolean }) =>
		(entries: ReadonlyArray<ProviderEntry>): Effect.Effect<void, unknown> =>
			provider.reconcile(def.name, entries, store).pipe(
				Effect.tap((echoed) => {
					if (!store || state.warned || echoed.length === 0) return Effect.void;
					if (echoed.some((e) => e.store === store)) return Effect.void;
					state.warned = true;
					return Effect.logWarning(
						`host is too old for store claims: this source's games will appear as custom ` +
							`entries and the host's own "${store}" scanner is not suppressed, so titles ` +
							`may be listed twice. Updating the host resolves it.`,
					);
				}),
				Effect.asVoid,
			);

	const main = Effect.gen(function* () {
		const cfgService = yield* config;
		const provider = yield* ProviderClient;
		const state = { warned: false };

		const engine = yield* makeSyncEngine<
			ScanReport,
			ReadonlyArray<ProviderEntry>,
			never
		>({
			compute: () => cfgService.load.pipe(Effect.flatMap(computeEntries)),
			apply: applyEntries(provider, state),
			// The host IS the state: a full-replace reconcile is idempotent, so there is nothing to
			// persist between runs. Reporting no previous fingerprint means the first sync after a
			// restart always pushes, which is exactly what we want (the host may have been reinstalled
			// underneath us).
			lastSync: { get: Effect.succeed(undefined), set: () => Effect.void },
			settings: cfgService.load.pipe(
				Effect.map((cfg) => def.watchDirs?.(cfg) ?? []),
				// A config file that won't decode must not stop the poll loop: fall back to no watch
				// dirs, keep syncing on the timer, and let the operator see the parse error in the
				// settings drawer (`GET /__config` reports it).
				Effect.catch(() => Effect.succeed([] as ReadonlyArray<string>)),
				Effect.map((watchDirs) => ({
					pollInterval: poll,
					watch: true,
					debounce,
					watchDirs,
				})),
			),
		});

		// The UI server exists ONLY to serve `__config` (and the SDK's `__health`): no `staticDir`,
		// no API. That is the whole "settings without an SPA" story (design D7, closing G8), and the
		// `library` category is what keeps six installed scanners out of the console's sidebar.
		yield* serveUi({
			title: def.title ?? def.name,
			category: "library",
			config: { schema: def.configSchema, service: cfgService },
		});

		yield* engine.start;
		// A saved settings change is exactly when a user expects the library to update — and it may
		// have changed `watchDirs`, so re-read settings rather than just re-syncing.
		yield* Effect.forkScoped(
			Stream.runForEach(cfgService.changes, () => engine.reconfigure),
		);
		yield* Effect.never;
	});

	const kitDef: PluginKitDef<never, ProviderClient> = {
		name: def.name,
		...(def.version !== undefined ? { version: def.version } : {}),
		layer: ProviderClient.layer,
		main: main as Effect.Effect<
			void,
			never,
			ProviderClient | HostClient | PluginInfo | never
		>,
	};

	const standardCommands: Record<string, CliCommand<ProviderClient>> = {
		detect: {
			summary: "report whether this launcher is installed on the host",
			// Offline on purpose: "is Steam here?" must be answerable without a running host.
			offline: true,
			run: () =>
				Effect.gen(function* () {
					const cfg = yield* (yield* config).load;
					console.log((yield* def.detect(cfg)) ? "present" : "absent");
				}),
		},
		scan: {
			summary: "scan and print what WOULD be synced (--preview for the JSON entries)",
			// Also offline: the point is to debug a scanner against real launcher files without
			// touching the host's library.
			offline: true,
			run: (argv) =>
				Effect.gen(function* () {
					const cfg = yield* (yield* config).load;
					const { entries, report } = yield* computeEntries(cfg);
					if (argv.includes("--preview")) {
						console.log(JSON.stringify(entries, null, 2));
					} else {
						console.log(
							`${report.present ? "present" : "absent"}: ${report.entries} games, ` +
								`${report.launchers} launcher entries`,
						);
					}
				}),
		},
		uninstall: {
			summary: "remove this source's games from the host and release its store claim",
			run: () =>
				Effect.gen(function* () {
					const provider = yield* ProviderClient;
					// The empty reconcile clears the entries; DELETE is what releases the CLAIM — and
					// releasing is what brings the host's own built-in scanner straight back.
					yield* provider.reconcile(def.name, [], undefined);
					yield* provider.remove(def.name);
					console.log(`${def.name}: entries removed, store claim released`);
				}),
		},
	};

	return {
		def: definePluginKit(kitDef),
		cli: (argv) =>
			runPluginCli({
				def: kitDef,
				commands: {
					...standardCommands,
					...(def.commands ?? {}),
				} as Record<string, CliCommand<ProviderClient>>,
				...(argv !== undefined ? { argv } : {}),
			}),
	};
};
