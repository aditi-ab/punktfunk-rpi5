// The Effect face of `servePluginUi`: build the plugin's local API from an HttpApi (or
// any HttpRouter route layers), mount it as the SDK server's `fetch` handler, and manage
// register/renew/deregister through Scope. Validated end-to-end by the phase-0 spike:
// core-only env layers, no platform package, SPA fallthrough preserved.
import { type PluginUiHandle, servePluginUi } from "@punktfunk/host";
import { Effect, FileSystem, Layer, Path, Schema, type Scope } from "effect";
import { Etag, HttpPlatform, HttpRouter } from "effect/unstable/http";
import type { ConfigService } from "./config.js";
import { UiServeError } from "./errors.js";
import {
	HostClient,
	type HostClientService,
	PluginInfo,
} from "./host-client.js";

/**
 * Everything `HttpApiBuilder.layer` needs beyond the router, satisfied from effect core —
 * plugins never pull a platform package for their UI API.
 */
export const httpApiEnv = Layer.provideMerge(
	Layer.mergeAll(Etag.layerWeak, Path.layer, HttpPlatform.layer),
	FileSystem.layerNoop({}),
);

/**
 * Derive a JSON Schema for a config schema, for the console's generic settings form.
 *
 * Returns `null` when derivation isn't possible, which the console reads as "render the raw JSON
 * editor instead" — the fallback that bounds this whole feature's risk.
 *
 * Authoring rules, verified against effect 4.0.0-beta.99 and pinned by
 * `test/library-config.test.ts` — if an effect upgrade changes any of them, that test fails:
 *
 * * Use `Schema.Finite` / `Schema.Int`, **never `Schema.Number`** — Number's *encoded* form admits
 *   the strings `"NaN"`/`"Infinity"`/`"-Infinity"`, so it derives a four-way `anyOf` that no sane
 *   form can render as a number input.
 * * A decoding default is an **Effect**: `withDecodingDefaultKey(Effect.succeed(true), …)`. Passing
 *   a bare thunk (`() => true`) still derives a schema and still type-checks, then dies at DECODE
 *   time with "Not a valid effect" — deriving is not evidence that the schema works.
 * * Annotate every field: `.annotate({ title, description, default })`. The derivation does NOT
 *   infer `default` from `withDecodingDefaultKey`, so an un-annotated field shows no placeholder.
 * * A *checked* schema (`Schema.Int`, or anything with `.check(...)`) nests its annotations and
 *   constraints under `allOf`, so a form must merge those branches, not read only the top level.
 * * `Schema.Literals([...])` derives a clean `enum` — prefer it over a union of strings. A union of
 *   non-literals derives an `anyOf`, which is the JSON-editor fallback case.
 * * Fields carrying `withDecodingDefaultKey(..., { encodingStrategy: "omit" })` correctly drop out
 *   of `required`, which is what keeps the raw file free of baked-in defaults.
 */
export const deriveConfigJsonSchema = (
	schema: Schema.Top,
): Record<string, unknown> | null => {
	try {
		const doc = Schema.toJsonSchemaDocument(schema as never);
		return doc as unknown as Record<string, unknown>;
	} catch {
		// A schema shape the derivation can't express (a transform, a recursive ref). The console
		// falls back to the JSON editor; the PUT still validates by decode, so nothing is lost but
		// the pretty form.
		return null;
	}
};

/** The plugin config surface the console's settings drawer drives. */
export interface ServeUiConfig<S extends Schema.Top> {
	/** The schema the raw file is validated against, and the form is derived from. */
	readonly schema: S;
	/** The config service (from `makeConfigService`) holding the raw round-trip semantics. */
	readonly service: ConfigService<S>;
}

/**
 * The `/__config` request handler, split out so it can be driven directly in tests (the wire shape
 * is the contract the console's settings drawer codes against — it deserves a real round-trip test,
 * not a mock).
 *
 * `ConfigService`'s effects are context-free by construction (the `PluginInfo` was resolved when the
 * service was built), so this runs them straight from a plain async handler.
 */
export const makeConfigHandler = <S extends Schema.Top>(
	cfg: ServeUiConfig<S>,
): ((req: Request) => Promise<Response>) => {
	// The derivation is stable for the life of the process — do it once, not per request.
	const schema = deriveConfigJsonSchema(cfg.schema);
	return async (req: Request): Promise<Response> => {
		if (req.method === "GET") {
			// A config file that fails to decode must not blank the whole drawer — answer with a
			// null value so the operator can still see (and replace) what is on disk.
			const value = await Effect.runPromise(cfg.service.loadRaw).catch(
				() => null,
			);
			return Response.json({ schema, value });
		}
		if (req.method === "PUT") {
			let body: unknown;
			try {
				body = await req.json();
			} catch (cause) {
				return Response.json(
					{ error: "body must be JSON", issue: String(cause) },
					{ status: 400 },
				);
			}
			try {
				// Validate-by-decode, persist RAW: `saveRaw` refuses a body the schema rejects and
				// never writes decoded defaults back into the operator's file.
				await Effect.runPromise(cfg.service.saveRaw(body));
				return Response.json({ ok: true });
			} catch (cause) {
				return Response.json(
					{ error: "config rejected", issue: String(cause) },
					{ status: 400 },
				);
			}
		}
		return new Response("method not allowed", { status: 405 });
	};
};

/** What a plugin answers when the host asks how to start one of its own library entries. */
export interface PluginLaunchTarget {
	/**
	 * The command LINE to run. The plugin composes AND quotes it — the host runs it as-is, so
	 * anything interpolated from untrusted input (a ROM filename) must already be quoted here.
	 */
	readonly command: string;
	/** Absolute working directory, for a program that resolves cores or configs relative to one. */
	readonly cwd?: string;
}

/**
 * The `/__launch` request handler, split out so it can be driven directly in tests — the wire shape
 * is a contract with the HOST, which deserves a real round-trip test rather than a mock.
 *
 * This is the plugin half of the `plugin` launch kind. A library entry published with
 * `launch: {kind: "plugin", value: "<key>"}` carries no command; when a client picks that tile, the
 * host asks the plugin that owns it — over this route, on the plugin's loopback UI port, with the
 * per-boot secret — what to run, and runs the answer itself (only the host can put the process
 * inside the captured session, and it needs the child to know when the game exits).
 *
 * **Answering `null` is load-bearing.** It becomes a 404, which is what the host gets for an entry
 * this plugin never published — and therefore what makes a library entry forged by someone holding a
 * stolen plugin token inert rather than arbitrary command execution. Resolve against your own state,
 * never by trusting the key.
 */
export const makeLaunchHandler = (
	resolve: (entry: string) => Effect.Effect<PluginLaunchTarget | null>,
): ((req: Request) => Promise<Response>) => {
	return async (req: Request): Promise<Response> => {
		if (req.method !== "POST") {
			return new Response("method not allowed", { status: 405 });
		}
		let body: unknown;
		try {
			body = await req.json();
		} catch (cause) {
			return Response.json(
				{ error: "body must be JSON", issue: String(cause) },
				{ status: 400 },
			);
		}
		const entry = (body as { entry?: unknown } | null)?.entry;
		if (typeof entry !== "string" || entry.length === 0) {
			return Response.json(
				{ error: "body must be {entry: string}" },
				{ status: 400 },
			);
		}
		let target: PluginLaunchTarget | null;
		try {
			target = await Effect.runPromise(resolve(entry));
		} catch (cause) {
			// A resolver that died is not the same as one that disowned the entry: keep 404 meaning
			// "not mine" so the host's log says which of the two happened.
			return Response.json(
				{ error: "launch resolution failed", issue: String(cause) },
				{ status: 500 },
			);
		}
		if (target === null) {
			return Response.json(
				{ error: `no launchable entry "${entry}"` },
				{ status: 404 },
			);
		}
		return Response.json({
			command: target.command,
			...(target.cwd !== undefined ? { cwd: target.cwd } : {}),
		});
	};
};

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
	 * What kind of plugin this is (`[a-z][a-z0-9-]{0,31}`). `"library"` keeps the plugin out of the
	 * console nav — its entry point is the Library section's Game sources surface instead.
	 */
	readonly category?: string;
	/**
	 * Serve `GET`/`PUT /__config` for the console's **generic settings form**, so a plugin with
	 * settings does not need to ship an SPA at all.
	 *
	 * `GET` answers `{schema, value}` — the derived JSON Schema (or `null`) and the raw,
	 * operator-authored config. `PUT` validates by decoding the body against the schema and, only
	 * then, persists it **raw**; defaults are never baked into the file. A rejected body comes back
	 * 400 with the decode issue.
	 *
	 * Auth is the existing per-boot UI secret — the console reaches this through its session-gated
	 * `/plugin-ui/<id>/…` proxy, so there is no new host surface and nothing new exposed to the LAN.
	 */
	readonly config?: ServeUiConfig<Schema.Top>;
	/**
	 * Serve `POST /__launch` — how a plugin answers "what do I run for this entry?" for library
	 * entries it published with `launch: {kind: "plugin", value: "<key>"}`.
	 *
	 * Set this when the plugin's tiles start something the host cannot name on its own (a ROM through
	 * an emulator, say). The alternative — publishing `kind: "command"` — is refused from the plugin
	 * lane outright: a stored command line is executed as the host user, and only the operator's own
	 * token may write one.
	 *
	 * Resolve against the plugin's OWN state and answer `null` for anything else; see
	 * {@link makeLaunchHandler} for why that 404 is the security-relevant case.
	 */
	readonly launch?: (entry: string) => Effect.Effect<PluginLaunchTarget | null>;
	/**
	 * The plugin API: `HttpApiBuilder.layer(api)` + group handler layers + raw routes
	 * (e.g. `sseRoute`), with plugin services already provided. `httpApiEnv` is provided
	 * here — only `HttpRouter` may remain open.
	 *
	 * Optional: a plugin whose only surface is `__config` (every library scanner) serves no API of
	 * its own, and omitting this leaves an empty router that 404s under `apiPrefix`.
	 */
	readonly api?: Layer.Layer<never, never, HttpRouter.HttpRouter>;
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
			Layer.provide(opts.api ?? Layer.empty, httpApiEnv),
		);
		yield* Effect.addFinalizer(() =>
			Effect.promise(() => dispose()).pipe(Effect.ignore),
		);

		const serveConfig = opts.config
			? makeConfigHandler(opts.config)
			: undefined;
		const serveLaunch = opts.launch
			? makeLaunchHandler(opts.launch)
			: undefined;

		const fetch = async (req: Request): Promise<Response | undefined> => {
			const url = new URL(req.url);
			// `__`-prefixed paths are the kit/SDK's own contract surface (`__health` lives in the
			// SDK), deliberately checked BEFORE the API prefix and before any static asset so a
			// plugin's own routes can never shadow them.
			if (url.pathname === "/__config") {
				return serveConfig?.(req) ?? new Response("not found", { status: 404 });
			}
			if (url.pathname === "/__launch") {
				return serveLaunch?.(req) ?? new Response("not found", { status: 404 });
			}
			if (!url.pathname.startsWith(prefix)) return undefined; // → static SPA
			return handler(req);
		};

		const handle = yield* Effect.acquireRelease(
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
						...(opts.category !== undefined ? { category: opts.category } : {}),
						fetch,
					}),
				catch: (cause) => new UiServeError({ cause }),
			}),
			(handle) => Effect.promise(() => handle.close()).pipe(Effect.ignore),
		);

		yield* verifyCategoryLanded(opts.category, info.name, host);
		return handle;
	});

/**
 * Read our own directory entry back and warn if the requested `category` is not on it.
 *
 * `category` travels through the UNTYPED `pf.request` seam precisely so an older host ignores it
 * instead of rejecting the registration — which means dropping it is SILENT by design, at three
 * different layers (an old host, an old runner-resolved SDK, a typo). On 2026-08-08 the middle one
 * happened: `@punktfunk/host@0.1.2` was published before it forwarded the field, so every installed
 * library scanner registered without a category. The visible result was Lutris and Heroic sitting in
 * the console nav — which they explicitly opt out of — and their settings unreachable, because the
 * Library section's Game sources surface lists exactly the plugins whose category IS `library`.
 * Nothing logged anything.
 *
 * So this asks the host what it actually recorded. Same spirit as the store-claim degradation
 * warning in `defineLibraryPlugin`: turn a silent no-op into one line that names the fix. Purely
 * advisory — a failed read, or a host too old to report the field, must never keep a working plugin
 * from starting.
 */
const verifyCategoryLanded = (
	category: string | undefined,
	id: string,
	host: { readonly request: HostClientService["request"] },
): Effect.Effect<void> => {
	if (category === undefined) return Effect.void;
	return host.request("GET", "/plugins").pipe(
		Effect.flatMap((body) => {
			const mine = (Array.isArray(body) ? body : []).find(
				(p): p is { id: string; category?: string } =>
					typeof p === "object" &&
					p !== null &&
					(p as { id?: unknown }).id === id,
			);
			// Not finding ourselves is not evidence of anything: the lease is registered
			// best-effort, so a host that was momentarily away simply has not listed us yet.
			if (!mine || mine.category === category) return Effect.void;
			return Effect.logWarning(
				`registered without category "${category}" (the host reports ` +
					`${mine.category === undefined ? "none" : `"${mine.category}"`}). ` +
					`This plugin will appear in the console's sidebar instead of its intended ` +
					`section. The usual cause is an @punktfunk/host older than 0.1.3, which drops ` +
					`the field before registering — update it, or the host, to resolve it.`,
			);
		}),
		// Advisory only: never let a diagnostic take down the plugin it is diagnosing.
		Effect.ignore,
	);
};
