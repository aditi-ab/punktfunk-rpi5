import { Link } from "@tanstack/react-router";
import {
	Activity,
	GaugeCircle,
	KeyRound,
	LibraryBig,
	MonitorPlay,
	ScrollText,
	Server,
	Settings,
} from "lucide-react";
import { motion, stagger } from "motion/react";
import type { ReactNode } from "react";
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
	{ to: "/settings", icon: Settings, label: () => m.nav_settings() },
] as const;

export function AppShell({ children }: { children: ReactNode }) {
	// Read the locale so the whole shell re-renders on a language switch.
	useLocale();
	return (
		<div className="flex min-h-screen">
			{/* Desktop sidebar (≥ sm). Sticky at viewport height: the page (body) scrolls with
			    long content, but the sidebar stays pinned — the explicit h-dvh stops the flex
			    stretch that would otherwise grow it (and push the language switcher) below the
			    fold. overflow-y-auto lets the nav itself scroll on very short viewports. */}
			<aside className="sticky top-0 hidden h-dvh w-60 shrink-0 flex-col overflow-y-auto border-r bg-card/40 p-4 sm:flex">
				<Link
					to="/"
					aria-label="punktfunk"
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
							activeOptions={{ exact: to === "/" }}
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
					{/* pb-24 leaves room for the fixed bottom nav on mobile. */}
					<div className="mx-auto max-w-[1700px] p-6 pb-24 sm:p-10 sm:pb-10">
						{children}
					</div>
				</main>
			</div>

			{/* Mobile bottom tab bar (< sm): the primary navigation on phones. */}
			<nav
				className="fixed inset-x-0 bottom-0 z-40 flex border-t bg-card/95 backdrop-blur sm:hidden"
				style={{ paddingBottom: "env(safe-area-inset-bottom)" }}
			>
				{NAV.map(({ to, icon: Icon, label }) => (
					<Link
						key={to}
						to={to}
						activeOptions={{ exact: to === "/" }}
						className="flex flex-1 flex-col items-center justify-center gap-1 px-0.5 py-2 text-muted-foreground transition-colors"
						activeProps={{ className: "text-[var(--brand-light)]" }}
					>
						<Icon className="size-5 shrink-0" />
						{/* Fixed two-line-tall box so a 1- or 2-line label keeps every icon
                at the same height (the labels vary by locale). */}
						<span className="flex h-7 w-full items-center justify-center text-center text-[10px] leading-tight">
							{label()}
						</span>
					</Link>
				))}
			</nav>
		</div>
	);
}

function LanguageSwitcher() {
	const current = useLocale();
	return (
		// biome-ignore lint/a11y/useSemanticElements: an aria-labelled role="group" is the right pattern for this small control cluster — no single semantic element fits.
		<div className="flex gap-1" role="group" aria-label="Language">
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
