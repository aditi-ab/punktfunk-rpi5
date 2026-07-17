// ── Example 4/4 · ADVANCED, Effect-native ───────────────────────────────────────────────────
// You do NOT need this to use the SDK — examples 1–3 (the plain Promise `connect()` facade) cover
// most automation. Reach for `@punktfunk/host/effect` only when you're composing Effect programs
// and want the event stream, typed errors, and structured interruption as first-class values.
//
// Here: when the living-room TV starts a stream, apply a display preset — as a single composed,
// typed, interruptible program (a `Stream` of events piped into a request, provided a live layer).
import { Cause, Effect, Stream } from "effect";
import { events, PunktfunkHost, PunktfunkHostLive } from "../src/effect.js";

const program = Effect.gen(function* () {
	const pf = yield* PunktfunkHost;
	yield* events().pipe(
		Stream.filter(
			(e) => e.kind === "stream.started" && e.stream.client === "Living Room TV",
		),
		Stream.runForEach(() =>
			pf
				.request("PUT", "/display/settings", { mode: "preset", preset: "couch" })
				.pipe(
					Effect.catchCause((cause) =>
						Effect.logWarning(`preset failed: ${Cause.pretty(cause)}`),
					),
				),
		),
	);
});

Effect.runPromise(program.pipe(Effect.provide(PunktfunkHostLive()))).catch(console.error);
