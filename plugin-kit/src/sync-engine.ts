// The generic sync engine — the poll/watch/debounce/coalesce/fingerprint machinery that
// was ~duplicated between rom-manager and playnite, as one Effect service.
//
// Semantics are a faithful port of the original Engine guard:
//   - single-flight: a sync while one runs records a pending trigger and returns
//     AlreadyRunning; the running pass re-fires once ("coalesced") when it finishes
//   - content fingerprint (sha256 of the entries JSON) skips the apply when unchanged
//   - interval poll + best-effort fs watchers (recursive where the OS supports it, top-dir
//     fallback on Linux) with debounce; the poll is the real safety net on SMB/NFS
//   - every transition publishes a SyncStatus (the UI's SSE feed)
// All loops live in a private scope that `reconfigure` closes and rebuilds, and the whole
// engine tears down with the Scope it was constructed in.
import { createHash } from "node:crypto";
import * as fs from "node:fs";
import {
	type Duration,
	Effect,
	Exit,
	PubSub,
	Queue,
	Ref,
	Scope,
	Stream,
} from "effect";
import { SyncError } from "./errors.js";

export type SyncReason =
	| "startup"
	| "poll"
	| "fs-change"
	| "config-change"
	| "manual"
	| "coalesced";

export interface LastSync {
	readonly fingerprint: string;
	readonly count: number;
	readonly at: number;
}

export type SyncOutcome<Report> =
	| {
			readonly _tag: "Applied";
			readonly report: Report;
			readonly count: number;
	  }
	| { readonly _tag: "Unchanged"; readonly report: Report }
	| { readonly _tag: "AlreadyRunning" };

export interface SyncStatus<Report> {
	readonly syncing: boolean;
	readonly lastReport?: Report;
	readonly lastSync?: LastSync;
}

export interface SyncSettings {
	readonly pollInterval: Duration.Duration;
	readonly watch: boolean;
	readonly debounce: Duration.Duration;
	readonly watchDirs: ReadonlyArray<string>;
}

export interface SyncEngineOptions<
	Report,
	Entries extends ReadonlyArray<unknown>,
	R,
> {
	/** Produce the full desired state. Pure of host effects — `apply` does the write. */
	readonly compute: (
		reason: SyncReason,
	) => Effect.Effect<
		{ readonly entries: Entries; readonly report: Report },
		unknown,
		R
	>;
	/** Push the desired state to the host (usually `ProviderClient.reconcile`). */
	readonly apply: (entries: Entries) => Effect.Effect<void, unknown, R>;
	/** Override the content fingerprint (default: sha256 of the entries JSON). */
	readonly fingerprint?: (entries: Entries) => string;
	/** Durable fingerprint storage (usually the plugin's CacheStore). */
	readonly lastSync: {
		readonly get: Effect.Effect<LastSync | undefined, never, R>;
		readonly set: (last: LastSync) => Effect.Effect<void, never, R>;
	};
	/** Re-read on every `reconfigure` — loops restart with fresh settings. */
	readonly settings: Effect.Effect<SyncSettings, never, R>;
}

export interface SyncEngine<Report> {
	readonly sync: (
		reason: SyncReason,
	) => Effect.Effect<SyncOutcome<Report>, SyncError>;
	readonly status: Effect.Effect<SyncStatus<Report>>;
	/** Emits on every sync start/finish — the UI's SSE feed. */
	readonly changes: Stream.Stream<SyncStatus<Report>>;
	/** Initial sync + start poll/watch loops (scoped to the construction Scope). */
	readonly start: Effect.Effect<void>;
	/** Restart loops with fresh settings, then sync("config-change"). */
	readonly reconfigure: Effect.Effect<void>;
}

const defaultFingerprint = (entries: unknown): string =>
	createHash("sha256").update(JSON.stringify(entries)).digest("hex");

/** Best-effort watcher set over `dirs`: recursive where supported, top-dir fallback. */
const openWatchers = (
	dirs: ReadonlyArray<string>,
	onEvent: () => void,
): fs.FSWatcher[] => {
	const out: fs.FSWatcher[] = [];
	for (const dir of dirs) {
		try {
			out.push(fs.watch(dir, { recursive: true }, onEvent));
		} catch {
			try {
				out.push(fs.watch(dir, onEvent));
			} catch {
				// unwatchable (poll covers it)
			}
		}
	}
	return out;
};

export const makeSyncEngine = <
	Report,
	Entries extends ReadonlyArray<unknown>,
	R,
>(
	opts: SyncEngineOptions<Report, Entries, R>,
): Effect.Effect<SyncEngine<Report>, never, R | Scope.Scope> =>
	Effect.gen(function* () {
		const ctx = yield* Effect.context<R>();
		const run = <A, E>(eff: Effect.Effect<A, E, R>): Effect.Effect<A, E> =>
			Effect.provide(eff, ctx);
		const fingerprint = opts.fingerprint ?? defaultFingerprint;

		const flags = yield* Ref.make({ syncing: false, pending: false });
		const lastReport = yield* Ref.make<Report | undefined>(undefined);
		const hub = yield* PubSub.unbounded<SyncStatus<Report>>();

		const status: Effect.Effect<SyncStatus<Report>> = Effect.gen(function* () {
			const f = yield* Ref.get(flags);
			return {
				syncing: f.syncing,
				lastReport: yield* Ref.get(lastReport),
				lastSync: yield* run(opts.lastSync.get),
			};
		});
		const publish = status.pipe(
			Effect.flatMap((s) => PubSub.publish(hub, s)),
			Effect.asVoid,
		);

		const doSync = (
			reason: SyncReason,
		): Effect.Effect<SyncOutcome<Report>, SyncError> =>
			Effect.gen(function* () {
				const { entries, report } = yield* run(opts.compute(reason)).pipe(
					Effect.mapError((cause) => new SyncError({ reason, cause })),
				);
				yield* Ref.set(lastReport, report);
				const fp = fingerprint(entries);
				const prev = yield* run(opts.lastSync.get);
				if (prev?.fingerprint === fp) {
					yield* Effect.log(
						`sync (${reason}): no changes (${entries.length} entries)`,
					);
					return { _tag: "Unchanged", report } as const;
				}
				yield* run(opts.apply(entries)).pipe(
					Effect.mapError((cause) => new SyncError({ reason, cause })),
				);
				yield* run(
					opts.lastSync.set({
						fingerprint: fp,
						count: entries.length,
						at: Date.now(),
					}),
				);
				yield* Effect.log(
					`sync (${reason}): reconciled ${entries.length} entries`,
				);
				return { _tag: "Applied", report, count: entries.length } as const;
			});

		// Errors must never kill a loop — log and carry on (the original's catch).
		const safeSync = (reason: SyncReason): Effect.Effect<void> =>
			sync(reason).pipe(
				Effect.catch((e: SyncError) =>
					Effect.logWarning(`sync (${reason}) failed: ${e.cause}`),
				),
				Effect.asVoid,
			);

		const sync = (
			reason: SyncReason,
		): Effect.Effect<SyncOutcome<Report>, SyncError> =>
			Ref.modify(flags, (f) =>
				f.syncing
					? ([false, { ...f, pending: true }] as const)
					: ([true, { ...f, syncing: true }] as const),
			).pipe(
				Effect.flatMap((acquired) => {
					if (!acquired) {
						return Effect.succeed({ _tag: "AlreadyRunning" } as const);
					}
					return publish.pipe(
						Effect.andThen(doSync(reason)),
						Effect.ensuring(
							Effect.gen(function* () {
								const pending = yield* Ref.modify(
									flags,
									(f) =>
										[f.pending, { syncing: false, pending: false }] as const,
								);
								yield* publish;
								if (pending) {
									yield* Effect.forkDetach(safeSync("coalesced"));
								}
							}),
						),
					);
				}),
			);

		// ---------------------------------------------------------------- loops
		const loopScope = yield* Ref.make<Scope.Closeable | undefined>(undefined);

		const stopLoops = Effect.gen(function* () {
			const prev = yield* Ref.getAndSet(loopScope, undefined);
			if (prev) yield* Scope.close(prev, Exit.void);
		});

		const startLoops: Effect.Effect<void> = Effect.gen(function* () {
			yield* stopLoops;
			const scope = yield* Scope.make();
			yield* Ref.set(loopScope, scope);
			const settings = yield* run(opts.settings);

			const pollLoop = Effect.forever(
				Effect.sleep(settings.pollInterval).pipe(
					Effect.andThen(safeSync("poll")),
				),
			);
			yield* Effect.forkIn(pollLoop, scope);

			if (settings.watch && settings.watchDirs.length > 0) {
				const watchStream = Stream.callback<void>((queue) =>
					Effect.acquireRelease(
						Effect.sync(() =>
							openWatchers(settings.watchDirs, () => {
								Queue.offerUnsafe(queue, undefined);
							}),
						),
						(watchers) =>
							Effect.sync(() => {
								for (const w of watchers) {
									try {
										w.close();
									} catch {
										// already closed
									}
								}
							}),
					),
				);
				const watchLoop = watchStream.pipe(
					Stream.debounce(settings.debounce),
					Stream.runForEach(() => safeSync("fs-change")),
				);
				yield* Effect.forkIn(watchLoop, scope);
			}
		});

		// Loop teardown rides the construction Scope (plugin shutdown).
		yield* Effect.addFinalizer(() => stopLoops);

		return {
			sync,
			status,
			changes: Stream.fromPubSub(hub),
			start: safeSync("startup").pipe(Effect.andThen(startLoops)),
			reconfigure: startLoops.pipe(Effect.andThen(safeSync("config-change"))),
		} satisfies SyncEngine<Report>;
	});
