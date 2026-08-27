import { QueryClient } from "@tanstack/react-query";
import { createRouter as createTanStackRouter } from "@tanstack/react-router";
import { ApiError } from "./api/fetcher";
import { routeTree } from "./routeTree.gen";

function createQueryClient() {
	return new QueryClient({
		defaultOptions: {
			queries: {
				staleTime: 2_000,
				// Don't hammer the host on auth/validation errors; do retry transient 5xx once.
				retry: (failureCount, error) => {
					if (
						error instanceof ApiError &&
						error.status >= 400 &&
						error.status < 500
					)
						return false;
					return failureCount < 1;
				},
			},
		},
	});
}

/**
 * The browser's ONE QueryClient.
 *
 * `getRouter()` can run more than once per page load (hydration discards and rebuilds the tree),
 * and a fresh client each time means a fresh, empty cache that nothing else holds a reference to.
 * That is how the event stream ended up invalidating a cache no component was reading: the
 * subscription captured the client from the first router, the live pages read the second one, and
 * every invalidation went to the dead one. One client per browser session fixes that and keeps the
 * cache across a router rebuild.
 *
 * Deliberately browser-only: on the SERVER every request must get its OWN client, or one visitor's
 * data would be served from another's cache.
 */
let browserQueryClient: QueryClient | undefined;

export function getRouter() {
	let queryClient: QueryClient;
	if (typeof window === "undefined") {
		queryClient = createQueryClient();
	} else {
		if (!browserQueryClient) browserQueryClient = createQueryClient();
		queryClient = browserQueryClient;
	}

	const router = createTanStackRouter({
		routeTree,
		context: { queryClient },
		defaultPreload: "intent",
		scrollRestoration: true,
		Wrap: ({ children }) => (
			<QueryProvider client={queryClient}>{children}</QueryProvider>
		),
	});

	reloadOnStaleChunk(router);

	return router;
}

const RELOAD_GUARD_KEY = "pf.stale-chunk-reload";

// Same reason the QueryClient above is a module-level singleton: hydration can build a
// second router and discard the first, so the listener reads whichever router is live at
// event time instead of capturing one at setup.
let liveRouter: { latestLocation: { href: string } } | undefined;

/**
 * Survive a deploy that lands while a tab is open.
 *
 * Every build hashes its chunk filenames and a deploy replaces the whole `.output`, so a
 * tab still holding the previous build's HTML asks for `/assets/*-<oldhash>.js` — which
 * the new server has never heard of. Routes are code-split, so that 404 surfaces on the
 * first navigation (or, with `defaultPreload: "intent"`, on the first hover) as a rejected
 * dynamic import that takes the console down.
 *
 * Vite raises `vite:preloadError` for exactly this case — its preload helper wraps both the
 * dependency preloads and the module import itself — and a full page load is the entire
 * fix, because the fresh HTML names the new chunks.
 *
 * Deliberately NOT `preventDefault()`: that suppresses Vite's rethrow and resolves the
 * import with `undefined`, handing the router a broken module on the way out.
 */
function reloadOnStaleChunk(router: { latestLocation: { href: string } }) {
	if (typeof window === "undefined") return;
	const alreadyListening = liveRouter !== undefined;
	liveRouter = router;
	if (alreadyListening) return;

	window.addEventListener("vite:preloadError", () => {
		// If the reload lands on HTML that STILL names missing chunks, stop: better to let
		// the error surface than to spin in a reload loop.
		try {
			const last = Number(sessionStorage.getItem(RELOAD_GUARD_KEY));
			if (Date.now() - last < 10_000) return;
			sessionStorage.setItem(RELOAD_GUARD_KEY, String(Date.now()));
		} catch {
			// Storage can be blocked outright (private mode, cookie policy). No guard then,
			// but a reload still beats a dead page.
		}

		// `latestLocation` is where the router was heading, so a click that tripped this
		// lands on the page the operator actually asked for. During a hover preload it is
		// the current URL, which makes this a plain reload.
		const target = liveRouter?.latestLocation.href;
		if (target) window.location.href = target;
		else window.location.reload();
	});
}

// Local import kept below the function so the module reads top-down.
import { QueryClientProvider } from "@tanstack/react-query";

function QueryProvider({
	client,
	children,
}: {
	client: QueryClient;
	children: React.ReactNode;
}) {
	return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

declare module "@tanstack/react-router" {
	interface Register {
		router: ReturnType<typeof getRouter>;
	}
}
