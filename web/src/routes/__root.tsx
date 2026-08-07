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
import { MotionConfig } from "motion/react";
import { useEffect } from "react";
import { AppShell } from "@/components/app-shell";
import { DialogsProvider } from "@/components/dialogs";
import { adoptStoredLocale, useLocale } from "@/lib/i18n";
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
			{ name: "theme-color", content: "#6c5bf3" },
			{ name: "apple-mobile-web-app-capable", content: "yes" },
			{ name: "apple-mobile-web-app-title", content: "Punktfunk" },
			{ title: "Punktfunk" },
		],
		links: [
			{ rel: "stylesheet", href: appCss },
			{ rel: "icon", type: "image/svg+xml", href: "/favicon.svg" },
			// Installable on a phone — this console is used from a couch as often as from a desk,
			// and a home-screen launcher beats retyping a LAN IP. Standalone display, no service
			// worker: an offline shell for a console whose every screen is live host state would
			// only ever show stale numbers convincingly.
			{ rel: "manifest", href: "/manifest.webmanifest" },
		],
	}),
	component: RootComponent,
});

function RootComponent() {
	// Adopt the persisted/browser locale AFTER hydration — the initial render stays at the base
	// locale to match SSR (see lib/i18n.ts), so this is the single, mismatch-free locale switch.
	useEffect(() => {
		adoptStoredLocale();
	}, []);
	// `lang` must track the locale the page is actually rendered in — it is what tells a screen
	// reader which pronunciation to use, and it was pinned to "en" while the app switched to German
	// underneath it. `adoptStoredLocale` also sets it on the live document; this keeps SSR honest.
	const locale = useLocale();
	// The login screen renders bare (no sidebar); everything else gets the app shell.
	const isLogin = useRouterState({
		select: (s) => s.location.pathname === "/login",
	});
	return (
		<html lang={locale} className="dark">
			<head>
				<HeadContent />
			</head>
			<body className="min-h-screen">
				{/* Motion defaults to `reducedMotion: "never"`, so every card, nav item and button
				    animated at full strength even for someone whose OS asks for less. "user" honours
				    the OS setting. */}
				<MotionConfig reducedMotion="user">
					{/* The console's own confirm/prompt, in place of the browser's grey boxes. Mounted
					    at the root because the navigation guard on the Displays page asks for one
					    while LEAVING that page — see components/dialogs.tsx. */}
					<DialogsProvider>
						{isLogin ? (
							<Outlet />
						) : (
							<AppShell>
								<Outlet />
							</AppShell>
						)}
					</DialogsProvider>
				</MotionConfig>
				{/* Sonner toaster (lazy client-side) — success feedback for auto-saved settings. */}
				<Toaster />
				<Scripts />
			</body>
		</html>
	);
}
