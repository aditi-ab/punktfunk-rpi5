import { QueryClient } from "@tanstack/react-query";
import { createRouter as createTanStackRouter } from "@tanstack/react-router";
import { ApiError } from "./api/fetcher";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
	const queryClient = new QueryClient({
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
