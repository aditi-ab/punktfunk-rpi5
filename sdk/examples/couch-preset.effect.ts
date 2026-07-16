// The RFC §7 quickstart, Effect-native: when the living-room TV starts a stream, apply a
// display preset — a composed, typed, interruptible program.
import { Effect, Stream } from "effect";
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
				.pipe(Effect.catchAll((e) => Effect.logWarning(`preset failed: ${e}`))),
		),
	);
});

Effect.runPromise(program.pipe(Effect.provide(PunktfunkHostLive()))).catch(console.error);
