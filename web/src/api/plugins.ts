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

/** Live plugin registrations, polled (and refetched on window focus) so the nav stays current. */
export function usePlugins() {
	return useQuery({
		queryKey: ["plugins"],
		queryFn: () => apiFetch<PluginSummary[]>("/api/v1/plugins"),
		refetchInterval: 30_000,
		refetchOnWindowFocus: true,
	});
}

/** Only the plugins that surface a UI — the ones that get a nav entry. */
export const uiPlugins = (list: PluginSummary[] | undefined): PluginSummary[] =>
	(list ?? []).filter((p) => p.ui);
