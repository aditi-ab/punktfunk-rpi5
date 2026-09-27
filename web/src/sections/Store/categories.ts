import type { StoreEntry } from "@/api/store";

/** The store's groups, in page order. An entry naming none of them lands in `other`. */
export const GROUPS = ["library", "metadata", "tools", "other"] as const;
export type Group = (typeof GROUPS)[number];

export const groupOf = (entry: StoreEntry): Group =>
	GROUPS.find((g) => entry.categories?.includes(g)) ?? "other";

/**
 * The non-empty groups in page order. Inside one, a launcher found on this host comes first, then
 * by title. `detected` is tri-state and only a positive probe lifts an entry.
 */
export function groupCatalog(entries: StoreEntry[]): [Group, StoreEntry[]][] {
	const sorted = [...entries].sort(
		(a, b) =>
			Number(b.detected === true) - Number(a.detected === true) ||
			a.title.localeCompare(b.title),
	);
	return GROUPS.map(
		(g) => [g, sorted.filter((e) => groupOf(e) === g)] as [Group, StoreEntry[]],
	).filter(([, list]) => list.length > 0);
}
