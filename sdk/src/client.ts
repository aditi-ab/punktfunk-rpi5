// The Effect-native surface (RFC §7): the `PunktfunkHost` service — a typed management-API
// client plus the lifecycle-event `Stream` — provided by [`PunktfunkHostLive`]. Wire shapes
// are Effect Schemas (generated for REST in ./gen/schemas.ts, hand-mirrored for events in
// ./wire.ts); API responses are validated by default, so host/SDK version skew surfaces as a
// typed [`VersionSkew`] instead of an `undefined` three frames later.
import {
	Context,
	Data,
	Effect,
	Layer,
	Option,
	Schema as S,
	Stream,
} from "effect";
import {
	type ConnectOptions,
	type ResolvedConfig,
	resolveConfig,
} from "./config.js";
import { HttpStatusError, httpRequest } from "./core.js";
import {
	type EventStreamOptions,
	type SseFrame,
	SseAuthError,
	sseFrames,
} from "./sse.js";
import { decodeHostEvent, type HostEvent } from "./wire.js";

/** Bad credentials — the token (or paired cert) was rejected. */
export class AuthError extends Data.TaggedError("AuthError")<{
	message: string;
}> {}
/** The host answered with a non-2xx (the message is its `ApiError` envelope). */
export class ApiError extends Data.TaggedError("ApiError")<{
	status: number;
	message: string;
}> {}
/** The request never completed (connection refused, TLS, abort). */
export class TransportError extends Data.TaggedError("TransportError")<{
	cause: unknown;
}> {}
/** A 2xx body did not match its schema — host and SDK disagree on the wire shape. */
export class VersionSkew extends Data.TaggedError("VersionSkew")<{
	path: string;
	issue: string;
}> {}
/** The event stream failed unrecoverably (auth) — transient trouble self-heals via reconnect. */
export class EventStreamError extends Data.TaggedError("EventStreamError")<{
	cause: unknown;
}> {}

export type RequestError = AuthError | ApiError | TransportError;

export interface PunktfunkHostService {
	readonly config: ResolvedConfig;
	/** One management-API request under `/api/v1`; the parsed JSON body. */
	readonly request: (
		method: string,
		path: string,
		body?: unknown,
	) => Effect.Effect<unknown, RequestError>;
	/** GET + schema-validate (the generated schemas from `@punktfunk/host/effect`'s `api`). */
	readonly get: <A, I>(
		path: string,
		schema: S.Schema<A, I>,
	) => Effect.Effect<A, RequestError | VersionSkew>;
	/**
	 * The lifecycle-event stream: decoded [`HostEvent`]s with automatic reconnect +
	 * `Last-Event-ID` resume. Unknown kinds and the `dropped` marker surface on
	 * [`eventsRaw`] (and the warning callback), never as a failure here.
	 */
	readonly events: (
		opts?: EventStreamOptions,
	) => Stream.Stream<HostEvent, EventStreamError>;
	/** Every SSE frame verbatim — the `dropped` marker and unknown kinds included. */
	readonly eventsRaw: (
		opts?: EventStreamOptions,
	) => Stream.Stream<SseFrame, EventStreamError>;
}

export class PunktfunkHost extends Context.Tag("@punktfunk/host/PunktfunkHost")<
	PunktfunkHost,
	PunktfunkHostService
>() {}

const toRequestError = (path: string, cause: unknown): RequestError => {
	if (cause instanceof HttpStatusError) {
		return cause.status === 401
			? new AuthError({ message: cause.message })
			: new ApiError({ status: cause.status, message: cause.message });
	}
	return new TransportError({ cause });
};

export const makeService = (cfg: ResolvedConfig): PunktfunkHostService => {
	const request = (method: string, path: string, body?: unknown) =>
		Effect.tryPromise({
			try: () => httpRequest(cfg, method, path, body),
			catch: (cause) => toRequestError(path, cause),
		});
	const get = <A, I>(path: string, schema: S.Schema<A, I>) =>
		request("GET", path).pipe(
			Effect.flatMap((body) =>
				S.decodeUnknown(schema)(body).pipe(
					Effect.mapError(
						(e) => new VersionSkew({ path, issue: String(e) }),
					),
				),
			),
		);
	const eventsRaw = (opts?: EventStreamOptions) =>
		// suspend: each run must get a FRESH generator (a generator is single-use).
		Stream.suspend(() =>
			Stream.fromAsyncIterable(
				sseFrames(cfg, opts),
				(cause) => new EventStreamError({ cause }),
			),
		);
	const events = (opts?: EventStreamOptions) => {
		const warn =
			opts?.onWarning ?? ((m: string) => console.warn(`[punktfunk] ${m}`));
		return eventsRaw(opts).pipe(
			Stream.filterMap((frame) => {
				if (frame.event === "dropped") {
					warn(
						"event cursor fell off the host's ring — resync via the REST snapshots",
					);
					return Option.none();
				}
				let json: unknown;
				try {
					json = JSON.parse(frame.data);
				} catch {
					warn(`unparseable event frame (${frame.event})`);
					return Option.none();
				}
				const decoded = decodeHostEvent(json);
				if (decoded._tag === "Left") {
					// An unknown kind from a NEWER host is expected (additive-only wire) —
					// it rides the raw channel; a consumer that wants it uses eventsRaw.
					warn(`unknown/undecodable event kind "${frame.event}"`);
					return Option.none();
				}
				return Option.some(decoded.right);
			}),
		);
	};
	return { config: cfg, request, get, events, eventsRaw };
};

/**
 * The live layer: resolves URL/token/CA (env → host files) and provides [`PunktfunkHost`].
 */
export const layer = (
	options?: ConnectOptions,
): Layer.Layer<PunktfunkHost, TransportError> =>
	Layer.effect(
		PunktfunkHost,
		Effect.tryPromise({
			try: () => resolveConfig(options),
			catch: (cause) => new TransportError({ cause }),
		}).pipe(Effect.map(makeService)),
	);

/** RFC-spelled alias of [`layer`]. */
export const PunktfunkHostLive = layer;

export { SseAuthError };
