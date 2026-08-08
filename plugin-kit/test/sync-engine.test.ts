// SyncEngine semantics: fingerprint skip, single-flight coalescing, status feed.
import { describe, expect, test } from "bun:test";
import { Duration, Effect, Fiber, Ref, type Scope, Stream } from "effect";
import {
	type LastSync,
	makeSyncEngine,
	type SyncOutcome,
} from "../src/index.js";

interface Report {
	readonly included: number;
}

const harness = (opts?: {
	computeDelayMs?: number;
	entries?: () => ReadonlyArray<string>;
}) =>
	Effect.gen(function* () {
		const applied = yield* Ref.make(0);
		const last = yield* Ref.make<LastSync | undefined>(undefined);
		const entries = opts?.entries ?? (() => ["a", "b"]);
		const engine = yield* makeSyncEngine<Report, ReadonlyArray<string>, never>({
			compute: () =>
				Effect.suspend(() => {
					const e = entries();
					return Effect.succeed({
						entries: e,
						report: { included: e.length },
					});
				}).pipe(
					opts?.computeDelayMs
						? Effect.delay(Duration.millis(opts.computeDelayMs))
						: (x) => x,
				),
			apply: () => Ref.update(applied, (n) => n + 1),
			lastSync: {
				get: Ref.get(last),
				set: (l) => Ref.set(last, l),
			},
			settings: Effect.succeed({
				pollInterval: Duration.minutes(60),
				watch: false,
				debounce: Duration.millis(10),
				watchDirs: [],
			}),
		});
		return { engine, applied, last };
	});

const run = <A>(eff: Effect.Effect<A, unknown, Scope.Scope>): Promise<A> =>
	Effect.runPromise(Effect.scoped(eff) as Effect.Effect<A>);

describe("SyncEngine", () => {
	test("first sync applies; unchanged content skips the apply", async () => {
		const { first, second, count } = await run(
			Effect.gen(function* () {
				const h = yield* harness();
				const first = yield* h.engine.sync("manual");
				const second = yield* h.engine.sync("manual");
				return {
					first,
					second,
					count: yield* Ref.get(h.applied),
				};
			}),
		);
		expect(first._tag).toBe("Applied");
		if (first._tag === "Applied") expect(first.count).toBe(2);
		expect(second._tag).toBe("Unchanged");
		expect(count).toBe(1);
	});

	test("changed content re-applies", async () => {
		let call = 0;
		const { outcomes, count } = await run(
			Effect.gen(function* () {
				const h = yield* harness({
					entries: () => (call++ === 0 ? ["a"] : ["a", "b"]),
				});
				const o1 = yield* h.engine.sync("manual");
				const o2 = yield* h.engine.sync("manual");
				return { outcomes: [o1, o2], count: yield* Ref.get(h.applied) };
			}),
		);
		expect(outcomes.map((o: SyncOutcome<Report>) => o._tag)).toEqual([
			"Applied",
			"Applied",
		]);
		expect(count).toBe(2);
	});

	test("concurrent trigger returns AlreadyRunning and coalesces into a follow-up", async () => {
		const { during, count } = await run(
			Effect.gen(function* () {
				const h = yield* harness({ computeDelayMs: 50 });
				const fiber = yield* Effect.forkChild(h.engine.sync("manual"));
				yield* Effect.sleep("10 millis");
				const during = yield* h.engine.sync("manual");
				yield* Fiber.join(fiber);
				// the coalesced re-run is forked detached — give it a beat
				yield* Effect.sleep("120 millis");
				return { during, count: yield* Ref.get(h.applied) };
			}),
		);
		expect(during._tag).toBe("AlreadyRunning");
		// first sync applied; the coalesced pass found identical content → skipped
		expect(count).toBe(1);
	});

	test("status feed publishes syncing transitions", async () => {
		const statuses = await run(
			Effect.gen(function* () {
				const h = yield* harness();
				const fiber = yield* Effect.forkChild(
					h.engine.changes.pipe(Stream.take(2), Stream.runCollect),
				);
				yield* Effect.sleep("20 millis");
				yield* h.engine.sync("manual");
				return yield* Fiber.join(fiber);
			}),
		);
		expect(statuses[0]?.syncing).toBe(true);
		expect(statuses[1]?.syncing).toBe(false);
		expect(statuses[1]?.lastSync?.count).toBe(2);
	});
});
