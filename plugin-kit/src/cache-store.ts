// Disposable derived state (cache.json): corrupt or absent falls back to `empty` and
// never fails a read; every `modify` is write-through with the same atomic-write
// discipline as config. Held in a Ref so reads are cheap and mutations are ordered.
import * as fs from "node:fs";
import { Effect, Ref, Schema } from "effect";
import type { ConfigWriteError } from "./errors.js";
import { PluginInfo } from "./host-client.js";
import { atomicWriteFile, ensureStateDir, statePath } from "./paths.js";

export interface CacheStore<S extends Schema.Top> {
	readonly get: Effect.Effect<S["Type"]>;
	/** Atomically update the cache (Ref + write-through). Returns the `modify` result. */
	readonly modify: <A>(
		f: (current: S["Type"]) => readonly [A, S["Type"]],
	) => Effect.Effect<A, ConfigWriteError>;
	/** `modify` without a result. */
	readonly update: (
		f: (current: S["Type"]) => S["Type"],
	) => Effect.Effect<void, ConfigWriteError>;
	/** Absolute path of the cache file (status views). */
	readonly path: string;
}

export const makeCacheStore = <S extends Schema.Top>(opts: {
	readonly schema: S;
	readonly empty: S["Type"];
	readonly fileName?: string;
}): Effect.Effect<CacheStore<S>, never, PluginInfo> =>
	Effect.gen(function* () {
		const info = yield* PluginInfo;
		const file = statePath(info.name, opts.fileName ?? "cache.json");

		const initial = yield* Effect.suspend(() => {
			try {
				const parsed = JSON.parse(fs.readFileSync(file, "utf8")) as unknown;
				return Schema.decodeUnknownEffect(opts.schema)(parsed).pipe(
					Effect.orElseSucceed(() => opts.empty),
				) as Effect.Effect<S["Type"]>;
			} catch {
				return Effect.succeed(opts.empty);
			}
		});
		const ref = yield* Ref.make<S["Type"]>(initial);

		const persist = (value: S["Type"]) =>
			ensureStateDir(info.name).pipe(
				Effect.flatMap(() => atomicWriteFile(file, JSON.stringify(value))),
			);

		const modify = <A>(f: (current: S["Type"]) => readonly [A, S["Type"]]) =>
			Ref.modify(ref, (current) => {
				const [a, next] = f(current);
				return [[a, next] as const, next] as const;
			}).pipe(Effect.flatMap(([a, next]) => persist(next).pipe(Effect.as(a))));

		return {
			get: Ref.get(ref),
			modify,
			update: (f) => modify((c) => [undefined, f(c)] as const),
			path: file,
		} satisfies CacheStore<S>;
	});
