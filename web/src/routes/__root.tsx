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
import { type CSSProperties, useEffect } from "react";
import { useUiConfig } from "@/api/uiConfig";
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
	// On an Omarchy box that opted in, follow the desktop's theme. Three raw values go in and
	// `data-omarchy` turns on the block in styles.css that expands them: `mode` picks the palette
	// the whole stylesheet already keys off, the accent re-tints the brand (and with it `--primary`,
	// `--accent`, `--ring` and the lens mark), and the background/foreground pair is what every
	// surface — cards, hovers, borders — is mixed out of. Everywhere else `theme` is null, the
	// attribute is absent and the console keeps its own violet, which is also what SSR renders and
	// what shows for the moment before this resolves.
	//
	// The expansion lives in CSS rather than here on purpose: `color-mix()` does it natively, in one
	// place, for both modes at once — and it is the only way `.dark`'s own values get overridden
	// without this component knowing which of them each mode uses.
	const { data: uiConfig } = useUiConfig();
	const theme = uiConfig?.theme ?? null;
	return (
		<html
			lang={locale}
			className={theme?.mode === "light" ? undefined : "dark"}
			data-omarchy={theme ? "" : undefined}
			style={
				theme
					? ({
							"--pf-accent": theme.accent,
							"--pf-bg": theme.background,
							"--pf-fg": theme.foreground,
						} as CSSProperties)
					: undefined
			}
		>
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
