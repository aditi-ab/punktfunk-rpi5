// The Effect face of `servePluginUi`: build the plugin's local API from an HttpApi (or
// any HttpRouter route layers), mount it as the SDK server's `fetch` handler, and manage
// register/renew/deregister through Scope. Validated end-to-end by the phase-0 spike:
// core-only env layers, no platform package, SPA fallthrough preserved.
import { type PluginUiHandle, servePluginUi } from "@punktfunk/host";
import { Effect, FileSystem, Layer, Path, Scope } from "effect";
import { Etag, HttpPlatform, HttpRouter } from "effect/unstable/http";
import { UiServeError } from "./errors.js";
import { HostClient, PluginInfo } from "./host-client.js";

/**
 * Everything `HttpApiBuilder.layer` needs beyond the router, satisfied from effect core —
 * plugins never pull a platform package for their UI API.
 */
export const httpApiEnv = Layer.provideMerge(
	Layer.mergeAll(Etag.layerWeak, Path.layer, HttpPlatform.layer),
	FileSystem.layerNoop({}),
);

export interface ServeUiOptions {
	/** Console nav title. */
	readonly title: string;
	/** lucide icon name for the console nav. */
	readonly icon?: string;
	/** Defaults to `PluginInfo.version`. */
	readonly version?: string;
	/** Built SPA directory (served with SPA fallback by the SDK). */
	readonly staticDir?: string | URL;
	/**
	 * The plugin API: `HttpApiBuilder.layer(api)` + group handler layers + raw routes
	 * (e.g. `sseRoute`), with plugin services already provided. `httpApiEnv` is provided
	 * here — only `HttpRouter` may remain open.
	 */
	readonly api: Layer.Layer<never, never, HttpRouter.HttpRouter>;
	/** Path prefix owned by the API handler (default "/api/"). */
	readonly apiPrefix?: string;
}

/**
 * Serve the plugin UI (scoped): the API handler and the loopback server come up together
 * and the release path deregisters from the host, stops the server, and disposes the
 * handler runtime — in that order.
 */
export const serveUi = (
	opts: ServeUiOptions,
): Effect.Effect<
	PluginUiHandle,
	UiServeError,
	HostClient | PluginInfo | Scope.Scope
> =>
	Effect.gen(function* () {
		const host = yield* HostClient;
		const info = yield* PluginInfo;
		const prefix = opts.apiPrefix ?? "/api/";

		const { handler, dispose } = HttpRouter.toWebHandler(
			Layer.provide(opts.api, httpApiEnv),
		);
		yield* Effect.addFinalizer(() =>
			Effect.promise(() => dispose()).pipe(Effect.ignore),
		);

		const fetch = async (req: Request): Promise<Response | undefined> => {
			const url = new URL(req.url);
			if (!url.pathname.startsWith(prefix)) return undefined; // → static SPA
			return handler(req);
		};

		return yield* Effect.acquireRelease(
			Effect.tryPromise({
				try: () =>
					servePluginUi(host.facade, {
						id: info.name,
						title: opts.title,
						...(opts.icon !== undefined ? { icon: opts.icon } : {}),
						...((opts.version ?? info.version) !== undefined
							? { version: opts.version ?? info.version }
							: {}),
						...(opts.staticDir !== undefined
							? { staticDir: opts.staticDir }
							: {}),
						fetch,
					}),
				catch: (cause) => new UiServeError({ cause }),
			}),
			(handle) => Effect.promise(() => handle.close()).pipe(Effect.ignore),
		);
	});
