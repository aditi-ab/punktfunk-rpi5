// The lifecycle-event wire (RFC §4/§7): the generated `HostEvent` union from the spec's
// `/api/v1/events` schema, the named refs its events carry, and the kind vocabulary. CI fails when
// the generated client and the spec disagree, so the event wire has no hand-written copy to drift.
// Additive-only within `schema: 1`: decoding tolerates unknown keys, and an unknown `kind`
// surfaces on the raw channel, never a throw.
import { Schema as S } from "effect";
import * as api from "./gen/punktfunk.js";

export {
	ClientRef,
	DeviceRef,
	DisconnectReason,
	GameEndReason,
	InputCounts,
	Plane,
	SessionEndReason,
	SessionRef,
	SessionSummary,
	StreamRef,
} from "./gen/punktfunk.js";

/** Every known lifecycle event — discriminated on `kind`. */
export const HostEvent = api.HostEvent;
export type HostEvent = api.HostEvent;

/** The known event kinds (for filters and the facade's `on()`). */
export type HostEventKind = HostEvent["kind"];

/** Narrow a HostEvent by kind: `EventOf<"stream.started">`. */
export type EventOf<K extends HostEventKind> = Extract<HostEvent, { kind: K }>;

/** A launched game, as the `game.*` events identify it. */
export const GameRef = api.GameRefPayload;
export type GameRef = api.GameRefPayload;

/** The settings preset a client dialled with. The id is stable across a rename. */
export type PresetRef = NonNullable<api.ClientRef["preset"]>;

/** What the encoder's target did. `avg_kbps` is a mean of the targets, not time-weighted. */
export type BitrateSpan = NonNullable<api.SessionSummary["bitrate"]>;
/** Gyro arrivals, and the gaps of 500 ms or more among them. */
export type GyroCadence = NonNullable<api.SessionSummary["gyro"]>;
/** Audio egress for the whole session, not the 30 s window the host log prints. */
export type AudioEgress = NonNullable<api.SessionSummary["audio"]>;

/**
 * Decode one event JSON into a [`HostEvent`], as a [`Result`]: `Success` for a known kind,
 * `Failure` for an unknown/undecodable one (a newer host — rides the raw channel, never throws).
 */
export const decodeHostEvent = S.decodeUnknownResult(HostEvent);

/**
 * Does `pattern` select `kind`? Exact kinds (`stream.started`) or `domain.*` prefixes on the
 * dot boundary — the same vocabulary as the host's SSE `?kinds=` filter and hooks `on:` field.
 */
export const kindMatches = (pattern: string, kind: string): boolean =>
	pattern.endsWith(".*")
		? kind.startsWith(pattern.slice(0, -1)) // "stream.*" → prefix "stream."
		: pattern === kind;
