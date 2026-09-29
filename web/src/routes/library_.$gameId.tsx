import { createFileRoute } from "@tanstack/react-router";
import { SectionLibraryEntry } from "@/sections/Library/Entry";

// `library_` keeps the page flat beside the grid rather than nested inside it. `$gameId` is a
// store-qualified id (`steam:570`), or `new`, which no library id can be.
export const Route = createFileRoute("/library_/$gameId")({
	validateSearch: (search: Record<string, unknown>): { tab?: string } =>
		typeof search.tab === "string" ? { tab: search.tab } : {},
	component: SectionLibraryEntry,
});
