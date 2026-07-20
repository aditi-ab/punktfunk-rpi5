// The library-provider client, owned by the kit so plugins stop hand-copying host calls.
// The wire SCHEMAS live in ./wire.ts (browser-safe — plugin contracts re-use them); the
// transport here stays the SDK's untyped `pf.request` seam (version-skew-safe under the
// runner-bundled SDK — design D7).
import { Context, Effect, Layer } from "effect";
import type { HostRequestError } from "./errors.js";
import { HostClient } from "./host-client.js";
import type { ProviderEntry } from "./wire.js";

export * from "./wire.js";

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
