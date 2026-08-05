// The library-provider client, owned by the kit so plugins stop hand-copying host calls.
// The wire SCHEMAS live in ./wire.ts (browser-safe — plugin contracts re-use them); the
// transport here stays the SDK's untyped `pf.request` seam (version-skew-safe under the
// runner-bundled SDK — design D7).
import { Context, Effect, Layer } from "effect";
import type { HostRequestError } from "./errors.js";
import { HostClient } from "./host-client.js";
import type { ProviderEntry } from "./wire.js";

export * from "./wire.js";

/** What the host echoed back for one reconciled entry — enough to tell whether a claim took. */
export interface ReconciledEntry {
	readonly id: string;
	readonly external_id?: string;
	/** The store badge the host assigned: the claim when it honoured one, else `"custom"`. */
	readonly store?: string;
}

export interface ProviderClientService {
	/**
	 * Full-replace reconcile: PUT the desired set; the host diffs by `external_id`.
	 *
	 * `store` claims that store for this provider (design D2), which is what makes the entries carry
	 * the store's own identity — deterministic `<store>:<external_id>` ids instead of opaque
	 * `custom:<id>` ones, the store's badge, and suppression of the host's matching built-in scanner
	 * so the two never double-list. One provider per store: a second claimant gets a 409.
	 *
	 * Returns the host's echoed entries so a caller can verify the claim actually took — a host
	 * predating claims ignores the query parameter silently, and the only way to notice is that the
	 * entries come back as `custom`.
	 */
	readonly reconcile: (
		providerId: string,
		entries: ReadonlyArray<ProviderEntry>,
		store?: string,
	) => Effect.Effect<ReadonlyArray<ReconciledEntry>, HostRequestError>;
	/**
	 * Remove every entry this provider owns **and release its store claim** (the explicit-uninstall
	 * path). Releasing is what brings the host's built-in scanner back.
	 */
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
					reconcile: (providerId, entries, store) =>
						host
							.request(
								"PUT",
								`/library/provider/${providerId}${
									store ? `?store=${encodeURIComponent(store)}` : ""
								}`,
								entries,
							)
							.pipe(
								// The host answers with its resulting entries. An older host may answer
								// with something else, so treat a non-array as "no echo" rather than
								// failing the sync.
								Effect.map((body) =>
									Array.isArray(body)
										? (body as ReadonlyArray<ReconciledEntry>)
										: [],
								),
							),
					remove: (providerId) =>
						host
							.request("DELETE", `/library/provider/${providerId}`)
							.pipe(Effect.asVoid),
				} satisfies ProviderClientService;
			}),
		);
}
