// The library-provider wire schemas — a browser-safe module (no node imports) so plugin
// CONTRACTS can share these types with their UIs. Mirrors the host's `ProviderEntryInput`
// (crates/punktfunk-host mgmt/library.rs). Identity codecs: plain JSON shapes, so values
// pass through unencoded; the value is the shared type + authoring validation.
import { Schema } from "effect";

export const Artwork = Schema.Struct({
	portrait: Schema.optionalKey(Schema.NullOr(Schema.String)),
	hero: Schema.optionalKey(Schema.NullOr(Schema.String)),
	logo: Schema.optionalKey(Schema.NullOr(Schema.String)),
	header: Schema.optionalKey(Schema.NullOr(Schema.String)),
});
export type Artwork = typeof Artwork.Type;

export const LaunchSpec = Schema.Struct({
	kind: Schema.Literal("command"),
	value: Schema.String,
});
export type LaunchSpec = typeof LaunchSpec.Type;

export const PrepStep = Schema.Struct({
	do: Schema.String,
	undo: Schema.optionalKey(Schema.NullOr(Schema.String)),
});
export type PrepStep = typeof PrepStep.Type;

export const ProviderEntry = Schema.Struct({
	external_id: Schema.String,
	title: Schema.String,
	art: Schema.optionalKey(Artwork),
	launch: Schema.optionalKey(Schema.NullOr(LaunchSpec)),
	prep: Schema.optionalKey(Schema.Array(PrepStep)),
});
export type ProviderEntry = typeof ProviderEntry.Type;
