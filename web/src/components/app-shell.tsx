import { Link, useRouterState } from "@tanstack/react-router";
import {
	Activity,
	GaugeCircle,
	KeyRound,
	LibraryBig,
	MonitorPlay,
	MoreHorizontal,
	Puzzle,
	ScrollText,
	Server,
	Settings,
	Workflow,
} from "lucide-react";
import { motion, stagger } from "motion/react";
import { type ReactNode, useState } from "react";
import { useHostEvents } from "@/api/events";
import { pluginIcon, uiPlugins, usePlugins } from "@/api/plugins";
import { BrandMark } from "@/components/brand-mark";
import { Wordmark } from "@/components/wordmark";
import { changeLocale, type Locale, locales, useLocale } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

const MLink = motion(Link);

const NAV = [
	{ to: "/", icon: Activity, label: () => m.nav_dashboard() },
	{ to: "/host", icon: Server, label: () => m.nav_host() },
	{ to: "/displays", icon: MonitorPlay, label: () => m.nav_displays() },
	{ to: "/library", icon: LibraryBig, label: () => m.nav_library() },
	{ to: "/stats", icon: GaugeCircle, label: () => m.nav_stats() },
	{ to: "/logs", icon: ScrollText, label: () => m.nav_logs() },
	{ to: "/pairing", icon: KeyRound, label: () => m.nav_pairing() },
	{ to: "/automation", icon: Workflow, label: () => m.nav_automation() },
	{ to: "/plugins", icon: Puzzle, label: () => m.nav_plugins() },
	{ to: "/settings", icon: Settings, label: () => m.nav_settings() },
] as const;

// "/plugins" is the store's index route and "/plugins/<id>" a plugin's own UI, so the store entry
// only counts as active on an exact match — otherwise it would light up alongside the plugin's own
// nav entry below. Same reason "/" is exact.
const EXACT: readonly string[] = ["/", "/plugins"];

// On phones a flat 8-tab bar is too cramped, so the first four are pinned and the rest live behind a
// "More" tab that opens a sheet above the bar. Keep it ≤ 5 slots including "More".
const MOBILE_PRIMARY = NAV.slice(0, 4);
const MOBILE_OVERFLOW = NAV.slice(4);

export function AppShell({ children }: { children: ReactNode }) {
	// Read the locale so the whole shell re-renders on a language switch.
	useLocale();
	// One subscription to the host's event stream for the whole console — it invalidates the queries
	// each event affects, so pages update on the transition instead of on their own timer. The
	// polling intervals stay as a floor in case the stream is unavailable.
	useHostEvents();
	return (
		<div className="flex min-h-screen">
			{/* Desktop sidebar (≥ sm). Sticky at viewport height: the page (body) scrolls with
			    long content, but the sidebar stays pinned — the explicit h-dvh stops the flex
			    stretch that would otherwise grow it (and push the language switcher) below the
			    fold. overflow-y-auto lets the nav itself scroll on very short viewports. */}
			<aside className="sticky top-0 hidden h-dvh w-60 shrink-0 flex-col overflow-y-auto border-r bg-card/40 p-4 sm:flex">
				<Link
					to="/"
					aria-label="Punktfunk"
					className="mb-7 flex items-center gap-2 px-2 pt-1"
				>
					<BrandMark className="size-7 drop-shadow-[0_2px_12px_rgba(108,91,243,0.45)]" />
					<Wordmark className="h-4" />
				</Link>
				<motion.nav
					animate="enter"
					initial="from"
					transition={{
						delayChildren: stagger(0.1),
					}}
					variants={{ enter: {}, from: {} }}
					className="flex flex-col gap-1"
				>
					{NAV.map(({ to, icon: Icon, label }) => (
						<MLink
							key={to}
							variants={{
								from: { opacity: 0, x: -20 },
								enter: { opacity: 1, x: 0 },
							}}
							whileHover={{ scale: 1.02 }}
							whileTap={{ scale: 0.98 }}
							to={to}
							activeOptions={{ exact: EXACT.includes(to) }}
							className="group relative flex items-center gap-3 rounded-md px-3 py-2 text-sm text-muted-foreground transition-colors hover:text-foreground"
							activeProps={{
								className: "bg-primary/15 text-foreground font-medium",
							}}
						>
							{/* Hover brightens: a brand-tinted wash layered OVER whatever the
                    link's background is (transparent or the active tint), so the
                    item gets lighter on hover — including the active one. */}
							<span
								aria-hidden
								className="pointer-events-none absolute inset-0 rounded-md bg-primary/0 transition-colors duration-200 group-hover:bg-primary/15"
							/>
							<Icon className="relative size-4" />
							<span className="relative">{label()}</span>
						</MLink>
					))}
				</motion.nav>
				<PluginNavSection />
				<div className="mt-auto pt-4">
					<LanguageSwitcher />
				</div>
			</aside>

			<div className="flex flex-1 flex-col overflow-x-hidden">
				{/* Mobile top bar (< sm): brand + language. The sidebar is hidden here. */}
				<header className="flex items-center gap-2 border-b bg-card/40 px-4 py-3 sm:hidden">
					<BrandMark className="size-6" />
					<Wordmark className="h-3.5" />
					<div className="ml-auto">
						<LanguageSwitcher />
					</div>
				</header>

				<main className="flex-1">
					{/* Mobile: a 16px side gutter (matching the top bar) so content isn't overly narrow;
					    pb-24 leaves room for the fixed bottom nav. Roomier padding from sm up. */}
					<div className="mx-auto max-w-[1700px] px-4 py-6 pb-24 sm:p-10 sm:pb-10">
						{children}
					</div>
				</main>
			</div>

			<MobileNav />
		</div>
	);
}

/** Desktop sidebar: the dynamic "Plugins" group, fed by the plugin directory. Renders nothing until
 * at least one plugin surfaces a UI — a host with no plugins sees zero extra chrome. */
function PluginNavSection() {
	const { data } = usePlugins();
	const plugins = uiPlugins(data);
	if (plugins.length === 0) return null;
	return (
		// Its own animation container, with the same variants + stagger as the main nav above. These
		// were plain links: the group sits OUTSIDE that `motion.nav`, so it inherited neither the
		// stagger nor the variants and plugin entries simply appeared. They arrive asynchronously
		// (and a fresh install adds one to a nav that is already on screen), which is exactly when
		// the animation earns its keep.
		<motion.div
			animate="enter"
			initial="from"
			transition={{ delayChildren: stagger(0.1) }}
			variants={{ enter: {}, from: {} }}
			className="mt-6 flex flex-col gap-1"
		>
			<motion.p
				variants={{ from: { opacity: 0 }, enter: { opacity: 1 } }}
				className="px-3 pb-1 text-xs font-medium uppercase tracking-wide text-muted-foreground/70"
			>
				{m.nav_plugins()}
			</motion.p>
			{plugins.map((p) => {
				const Icon = pluginIcon(p.ui?.icon);
				return (
					// The motion wrapper is a DIV around the link, not `motion(Link)`: wrapping Link
					// erases TanStack's typed `params`, and these entries need `$pluginId`.
					<motion.div
						key={p.id}
						variants={{
							from: { opacity: 0, x: -20 },
							enter: { opacity: 1, x: 0 },
						}}
						whileHover={{ scale: 1.02 }}
						whileTap={{ scale: 0.98 }}
					>
						<Link
							to="/plugins/$pluginId/$"
							params={{ pluginId: p.id, _splat: "" }}
							className="group relative flex items-center gap-3 rounded-md px-3 py-2 text-sm text-muted-foreground transition-colors hover:text-foreground"
							activeProps={{
								className: "bg-primary/15 text-foreground font-medium",
							}}
						>
							<span
								aria-hidden
								className="pointer-events-none absolute inset-0 rounded-md bg-primary/0 transition-colors duration-200 group-hover:bg-primary/15"
							/>
							<Icon className="relative size-4" />
							<span className="relative truncate">{p.title}</span>
						</Link>
					</motion.div>
				);
			})}
		</motion.div>
	);
}

/** Mobile bottom navigation (< sm): four pinned tabs + a "More" tab whose sheet holds the rest. */
function MobileNav() {
	const [moreOpen, setMoreOpen] = useState(false);
	const pathname = useRouterState({ select: (s) => s.location.pathname });
	const { data } = usePlugins();
	const plugins = uiPlugins(data);
	// Highlight "More" when the current route lives in the overflow — plugins included.
	const overflowActive =
		pathname.startsWith("/plugins/") ||
		MOBILE_OVERFLOW.some(
			(n) => pathname === n.to || pathname.startsWith(`${n.to}/`),
		);
	// Fixed two-line-tall label box so a 1- or 2-line label (labels vary by locale) keeps every icon
	// at the same height.
	const tab =
		"flex flex-1 flex-col items-center justify-center gap-1 px-0.5 py-2 text-muted-foreground transition-colors";
	const lbl =
		"flex h-7 w-full items-center justify-center text-center text-[10px] leading-tight";
	return (
		<>
			{/* Tap-outside backdrop, under the bar (z-50) but over the page. */}
			{moreOpen && (
				<button
					type="button"
					aria-label={m.nav_close_menu()}
					className="fixed inset-0 z-40 bg-black/40 sm:hidden"
					onClick={() => setMoreOpen(false)}
				/>
			)}
			<nav
				className="fixed inset-x-0 bottom-0 z-50 sm:hidden"
				style={{ paddingBottom: "env(safe-area-inset-bottom)" }}
			>
				{/* The "More" sheet sits directly above the bar (bottom-full of the fixed nav). */}
				{moreOpen && (
					<div className="absolute inset-x-0 bottom-full border-t bg-card/95 backdrop-blur">
						<div className="grid grid-cols-4 gap-1 p-2">
							{MOBILE_OVERFLOW.map(({ to, icon: Icon, label }) => (
								<Link
									key={to}
									to={to}
									onClick={() => setMoreOpen(false)}
									activeOptions={{ exact: EXACT.includes(to) }}
									className={cn(tab, "rounded-md")}
									activeProps={{ className: "text-[var(--brand-light)]" }}
								>
									<Icon className="size-5 shrink-0" />
									<span className={lbl}>{label()}</span>
								</Link>
							))}
							{plugins.map((p) => {
								const Icon = pluginIcon(p.ui?.icon);
								return (
									<Link
										key={p.id}
										to="/plugins/$pluginId/$"
										params={{ pluginId: p.id, _splat: "" }}
										onClick={() => setMoreOpen(false)}
										className={cn(tab, "rounded-md")}
										activeProps={{ className: "text-[var(--brand-light)]" }}
									>
										<Icon className="size-5 shrink-0" />
										<span className={lbl}>{p.title}</span>
									</Link>
								);
							})}
						</div>
					</div>
				)}
				<div className="flex border-t bg-card/95 backdrop-blur">
					{MOBILE_PRIMARY.map(({ to, icon: Icon, label }) => (
						<Link
							key={to}
							to={to}
							onClick={() => setMoreOpen(false)}
							activeOptions={{ exact: EXACT.includes(to) }}
							className={tab}
							activeProps={{ className: "text-[var(--brand-light)]" }}
						>
							<Icon className="size-5 shrink-0" />
							<span className={lbl}>{label()}</span>
						</Link>
					))}
					<button
						type="button"
						onClick={() => setMoreOpen((o) => !o)}
						aria-expanded={moreOpen}
						className={cn(
							tab,
							(moreOpen || overflowActive) && "text-[var(--brand-light)]",
						)}
					>
						<MoreHorizontal className="size-5 shrink-0" />
						<span className={lbl}>{m.nav_more()}</span>
					</button>
				</div>
			</nav>
		</>
	);
}

function LanguageSwitcher() {
	const current = useLocale();
	return (
		// biome-ignore lint/a11y/useSemanticElements: an aria-labelled role="group" is the right pattern for this small control cluster — no single semantic element fits.
		<div className="flex gap-1" role="group" aria-label={m.settings_language()}>
			{locales.map((l: Locale) => (
				<button
					key={l}
					type="button"
					onClick={() => changeLocale(l)}
					className={cn(
						"rounded px-2 py-1 text-xs uppercase transition-colors",
						l === current
							? "bg-primary/20 text-foreground font-medium"
							: "text-muted-foreground hover:text-foreground",
					)}
				>
					{l}
				</button>
			))}
		</div>
	);
}
