/// <reference types="vite/client" />

import type { QueryClient } from "@tanstack/react-query";
import {
	createRootRouteWithContext,
	HeadContent,
	Outlet,
	Scripts,
	useRouterState,
} from "@tanstack/react-router";
import "@fontsource-variable/geist";
import { Toaster } from "@unom/ui/toast";
import { AppShell } from "@/components/app-shell";
import appCss from "@/styles.css?url";

export interface RouterContext {
	queryClient: QueryClient;
}

export const Route = createRootRouteWithContext<RouterContext>()({
	head: () => ({
		meta: [
			{ charSet: "utf-8" },
			{ name: "viewport", content: "width=device-width, initial-scale=1" },
			{ name: "color-scheme", content: "dark light" },
			{ title: "Punktfunk" },
		],
		links: [
			{ rel: "stylesheet", href: appCss },
			{ rel: "icon", type: "image/svg+xml", href: "/favicon.svg" },
		],
	}),
	component: RootComponent,
});

function RootComponent() {
	// The login screen renders bare (no sidebar); everything else gets the app shell.
	const isLogin = useRouterState({
		select: (s) => s.location.pathname === "/login",
	});
	return (
		<html lang="en" className="dark">
			<head>
				<HeadContent />
			</head>
			<body className="min-h-screen">
				{isLogin ? (
					<Outlet />
				) : (
					<AppShell>
						<Outlet />
					</AppShell>
				)}
				{/* Sonner toaster (lazy client-side) — success feedback for auto-saved settings. */}
				<Toaster />
				<Scripts />
			</body>
		</html>
	);
}
