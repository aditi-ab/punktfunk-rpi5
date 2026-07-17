// `@punktfunk/host/effect` — the Effect-native surface (RFC §7): the `PunktfunkHost` service
// tag + live layer, the wire schemas, typed errors, and Stream accessors. The package root
// (`@punktfunk/host`) is the Promise facade; this entry is for plugins and Effect programs.
//
//   import { Effect, Stream } from "effect";
//   import { PunktfunkHost, PunktfunkHostLive, events } from "@punktfunk/host/effect";
//
//   const program = Effect.gen(function* () {
//     yield* events().pipe(
//       Stream.filter((e) => e.kind === "stream.started"),
//       Stream.runForEach((e) => Effect.log(`stream started: ${e.stream.mode}`)),
//     );
//   });
//   Effect.runPromise(program.pipe(Effect.provide(PunktfunkHostLive())));
import { Effect, type Schema as S, Stream } from "effect";
import {
	EventStreamError,
	PunktfunkHost,
	type RequestError,
	type VersionSkew,
} from "./client.js";
import type { EventStreamOptions, SseFrame } from "./sse.js";
import type { HostEvent } from "./wire.js";

export {
	ApiError,
	AuthError,
	EventStreamError,
	layer,
	makeService,
	PunktfunkHost,
	type PunktfunkHostService,
	PunktfunkHostLive,
	type RequestError,
	SseAuthError,
	TransportError,
	VersionSkew,
} from "./client.js";
export { type ConnectOptions, configDir, resolveConfig } from "./config.js";
export type { EventStreamOptions, SseFrame } from "./sse.js";
export * from "./wire.js";
/**
 * The generated REST surface: wire Schemas plus a typed `HttpClient`-based client (`make`),
 * from `@effect/openapi-generator` over `api/openapi.json`. Regenerate with `bun run gen`.
 */
export * as api from "./gen/punktfunk.js";

/** The decoded lifecycle-event stream of the ambient [`PunktfunkHost`]. */
export const events = (
	opts?: EventStreamOptions,
): Stream.Stream<HostEvent, EventStreamError, PunktfunkHost> =>
	Stream.unwrap(Effect.map(PunktfunkHost, (s) => s.events(opts)));

/** Every SSE frame verbatim (the `dropped` marker + unknown kinds included). */
export const eventsRaw = (
	opts?: EventStreamOptions,
): Stream.Stream<SseFrame, EventStreamError, PunktfunkHost> =>
	Stream.unwrap(Effect.map(PunktfunkHost, (s) => s.eventsRaw(opts)));

/** One management-API request under `/api/v1` on the ambient service. */
export const request = (
	method: string,
	path: string,
	body?: unknown,
): Effect.Effect<unknown, RequestError, PunktfunkHost> =>
	Effect.flatMap(PunktfunkHost, (s) => s.request(method, path, body));

/** GET + schema-validate on the ambient service (schemas from [`api`]). */
export const get = <A, I>(
	path: string,
	schema: S.Codec<A, I>,
): Effect.Effect<A, RequestError | VersionSkew, PunktfunkHost> =>
	Effect.flatMap(PunktfunkHost, (s) => s.get(path, schema));
