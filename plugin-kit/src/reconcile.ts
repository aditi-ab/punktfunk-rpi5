// The library-provider wire, owned by the kit so plugins stop hand-copying `wire.ts`.
// Schemas mirror the host's `ProviderEntryInput` (crates/punktfunk-host mgmt/library.rs);
// the transport stays the SDK's untyped `pf.request` seam (version-skew-safe under the
// runner-bundled SDK — design D7). These schemas are identity codecs (plain JSON shapes),
// so entries pass through unencoded; the value is the shared type + authoring validation.
import { Context, Effect, Layer, Schema } from "effect";
import type { HostRequestError } from "./errors.js";
import { HostClient } from "./host-client.js";

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

export interface ProviderClientService {
	/** Full-replace reconcile: PUT the desired set; the host diffs by `external_id`. */
	readonly reconcile: (
		providerId: string,
		entries: ReadonlyArray<ProviderEntry>,
	) => Effect.Effect<void, HostRequestError>;
	/** Remove every entry this provider owns (the explicit-uninstall path). */
	readonly remove: (providerId: string) => Effect.Effect<void, HostRequestError>;
}

export class ProviderClient extends Context.Service<
	ProviderClient,
	ProviderClientService
>()("@punktfunk/plugin-kit/ProviderClient") {
	static readonly layer: Layer.Layer<ProviderClient, never, HostClient> =
		Layer.effect(ProviderClient)(
			Effect.gen(function* () {
				const host = yield* HostClient;
				return {
					reconcile: (providerId, entries) =>
						host
							.request("PUT", `/library/provider/${providerId}`, entries)
							.pipe(Effect.asVoid),
					remove: (providerId) =>
						host
							.request("DELETE", `/library/provider/${providerId}`)
							.pipe(Effect.asVoid),
				} satisfies ProviderClientService;
			}),
		);
}
