// The plugin directory the console reads to grow its nav (plugin-ui-surface §5). This is a
// hand-written client (not orval-generated) so the nav works without regenerating the API client
// for the new endpoints; it rides the same `/api` BFF path as every other call, so the bearer token
// is injected server-side and the browser only ever sends its session cookie.
import { useQuery } from "@tanstack/react-query";
import {
	Blocks,
	Boxes,
	Clapperboard,
	Database,
	FolderCog,
	Gamepad2,
	Home,
	type LucideIcon,
	Plug,
	Puzzle,
	Wrench,
} from "lucide-react";
import { apiFetch } from "@/api/fetcher";

export interface PluginUiSummary {
	port: number;
	icon?: string;
}

export interface PluginSummary {
	id: string;
	title: string;
	version?: string;
	/** Present iff the plugin serves a UI (and thus gets a nav entry). */
	ui?: PluginUiSummary;
}

// A curated lucide set for plugin nav icons. Importing lucide's full dynamic icon map would defeat
// tree-shaking (U-S4), so a plugin picks a name from here; anything unknown falls back to Puzzle.
const ICONS: Record<string, LucideIcon> = {
	"gamepad-2": Gamepad2,
	puzzle: Puzzle,
	wrench: Wrench,
	database: Database,
	home: Home,
	blocks: Blocks,
	boxes: Boxes,
	plug: Plug,
	"folder-cog": FolderCog,
	clapperboard: Clapperboard,
};

/**
 * Resolve a registered icon name to a component (Puzzle fallback).
 *
 * `name` comes from a plugin's own registration, so it is untrusted input to a lookup on a plain
 * object — and a plain object inherits from Object.prototype. `ICONS["constructor"]` is `Object`,
 * which is truthy, so a `?? Puzzle` fallback never fires and React is handed `Object` as a
 * component: it throws out of render, and because this runs inside the AppShell nav that takes
 * down every page of the console. `Object.hasOwn` keeps the lookup to keys we actually declared.
 */
export const pluginIcon = (name?: string): LucideIcon => {
	if (!name || !Object.hasOwn(ICONS, name)) return Puzzle;
	return ICONS[name] ?? Puzzle;
};

/** The query key for the plugin directory — the nav is built from it. */
export const PLUGINS_KEY = ["plugins"] as const;

const IDLE_POLL_MS = 30_000;
const BOOST_POLL_MS = 2_000;
/** How long to keep polling fast after something changed the installed set. */
const BOOST_MS = 60_000;

/**
 * Until this timestamp, poll the directory fast.
 *
 * A finished install is NOT the moment the plugin appears: the host restarts the scripting runner
 * afterwards, and the plugin only registers its UI once that comes back — several seconds later,
 * and after any one-shot invalidation has already run and found the old list. So the nav sat
 * unchanged until the 30 s idle poll happened to land, which in practice meant "until I reloaded".
 *
 * Module-level rather than component state because the two things that need to trigger it (a store
 * job settling, a `plugins.changed`/`store.changed` event) both live outside the nav.
 */
let boostUntil = 0;

/** Poll the plugin directory fast for a while — call after anything that changes what's installed. */
export function boostPluginPolling(): void {
	boostUntil = Date.now() + BOOST_MS;
}

/** Live plugin registrations, polled (and refetched on window focus) so the nav stays current. */
export function usePlugins() {
	return useQuery({
		queryKey: PLUGINS_KEY,
		queryFn: () => apiFetch<PluginSummary[]>("/api/v1/plugins"),
		refetchInterval: () =>
			Date.now() < boostUntil ? BOOST_POLL_MS : IDLE_POLL_MS,
		refetchOnWindowFocus: true,
	});
}

/** Only the plugins that surface a UI — the ones that get a nav entry. */
export const uiPlugins = (list: PluginSummary[] | undefined): PluginSummary[] =>
	(list ?? []).filter((p) => p.ui);
