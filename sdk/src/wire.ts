// The lifecycle-event wire schemas (RFC §4/§7) — hand-written as a discriminated union on
// `kind` for precise types and decode errors. The REST surface is generated (./gen/schemas.ts);
// the event stream's `text/event-stream` payload is not expressible there, and the host's
// Rust-side JSON snapshot tests (crates/punktfunk-host/src/events.rs) are the wire's source of
// truth — this file mirrors them. Additive-only within `schema: 1`: decoding tolerates unknown
// keys (Effect's default), and an unknown `kind` surfaces on the raw channel, never a throw.
import { Schema as S } from "effect";

export const Plane = S.Literal("native", "gamestream");
export type Plane = S.Schema.Type<typeof Plane>;

export const DisconnectReason = S.Literal("quit", "timeout", "error");
export type DisconnectReason = S.Schema.Type<typeof DisconnectReason>;

export const ClientRef = S.Struct({
	name: S.String,
	fingerprint: S.optional(S.String),
	plane: Plane,
});
export type ClientRef = S.Schema.Type<typeof ClientRef>;

export const SessionRef = S.Struct({
	id: S.Number,
	client: S.String,
	mode: S.String,
	hdr: S.Boolean,
});
export type SessionRef = S.Schema.Type<typeof SessionRef>;

export const StreamRef = S.Struct({
	mode: S.String,
	hdr: S.Boolean,
	client: S.String,
	app: S.optional(S.String),
	plane: Plane,
});
export type StreamRef = S.Schema.Type<typeof StreamRef>;

export const DeviceRef = S.Struct({
	name: S.String,
	fingerprint: S.String,
	plane: Plane,
});
export type DeviceRef = S.Schema.Type<typeof DeviceRef>;

/** The `{seq, ts_ms, schema}` envelope every event carries. */
const envelope = {
	seq: S.Number,
	ts_ms: S.Number,
	schema: S.Number,
} as const;

export const ClientConnected = S.Struct({
	...envelope,
	kind: S.Literal("client.connected"),
	client: ClientRef,
});
export const ClientDisconnected = S.Struct({
	...envelope,
	kind: S.Literal("client.disconnected"),
	client: ClientRef,
	reason: DisconnectReason,
});
export const SessionStarted = S.Struct({
	...envelope,
	kind: S.Literal("session.started"),
	session: SessionRef,
});
export const SessionEnded = S.Struct({
	...envelope,
	kind: S.Literal("session.ended"),
	session: SessionRef,
});
export const StreamStarted = S.Struct({
	...envelope,
	kind: S.Literal("stream.started"),
	stream: StreamRef,
});
export const StreamStopped = S.Struct({
	...envelope,
	kind: S.Literal("stream.stopped"),
	stream: StreamRef,
});
export const PairingPending = S.Struct({
	...envelope,
	kind: S.Literal("pairing.pending"),
	device: DeviceRef,
});
export const PairingCompleted = S.Struct({
	...envelope,
	kind: S.Literal("pairing.completed"),
	device: DeviceRef,
});
export const PairingDenied = S.Struct({
	...envelope,
	kind: S.Literal("pairing.denied"),
	device: DeviceRef,
});
export const DisplayCreated = S.Struct({
	...envelope,
	kind: S.Literal("display.created"),
	backend: S.String,
	mode: S.String,
});
export const DisplayReleased = S.Struct({
	...envelope,
	kind: S.Literal("display.released"),
	count: S.Number,
});
export const LibraryChanged = S.Struct({
	...envelope,
	kind: S.Literal("library.changed"),
	source: S.String,
});
export const HostStarted = S.Struct({
	...envelope,
	kind: S.Literal("host.started"),
	version: S.String,
	gamestream: S.Boolean,
});
export const HostStopping = S.Struct({
	...envelope,
	kind: S.Literal("host.stopping"),
});

/** Every known lifecycle event — discriminated on `kind`. */
export const HostEvent = S.Union(
	ClientConnected,
	ClientDisconnected,
	SessionStarted,
	SessionEnded,
	StreamStarted,
	StreamStopped,
	PairingPending,
	PairingCompleted,
	PairingDenied,
	DisplayCreated,
	DisplayReleased,
	LibraryChanged,
	HostStarted,
	HostStopping,
);
export type HostEvent = S.Schema.Type<typeof HostEvent>;

/** The known event kinds (for filters and the facade's `on()`). */
export type HostEventKind = HostEvent["kind"];

/** Narrow a HostEvent by kind: `EventOf<"stream.started">`. */
export type EventOf<K extends HostEventKind> = Extract<HostEvent, { kind: K }>;

export const decodeHostEvent = S.decodeUnknownEither(HostEvent);

/**
 * Does `pattern` select `kind`? Exact kinds (`stream.started`) or `domain.*` prefixes on the
 * dot boundary — the same vocabulary as the host's SSE `?kinds=` filter and hooks `on:` field.
 */
export const kindMatches = (pattern: string, kind: string): boolean =>
	pattern.endsWith(".*")
		? kind.startsWith(pattern.slice(0, -1)) // "stream.*" → prefix "stream."
		: pattern === kind;
