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

	return createTanStackRouter({
		routeTree,
		context: { queryClient },
		defaultPreload: "intent",
		scrollRestoration: true,
		Wrap: ({ children }) => (
			<QueryProvider client={queryClient}>{children}</QueryProvider>
		),
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
