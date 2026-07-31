import { AlertTriangle, Ban, Check, Download, Search } from "lucide-react";
import { type FC, useMemo, useState } from "react";
import { pluginIcon } from "@/api/plugins";
import { type StoreEntry, useStoreCatalog } from "@/api/store";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { RunnerBanner } from "./Runner";
import { SourceChip, TierBadge } from "./TierBadge";

/** Case-insensitive substring match across the fields an operator would actually search by. */
function matches(entry: StoreEntry, needle: string): boolean {
	if (!needle) return true;
	const q = needle.toLowerCase();
	return [entry.title, entry.description, entry.pkg, entry.author].some((f) =>
		f.toLowerCase().includes(q),
	);
}

/**
 * Container: the catalog. Owns the catalog query plus the local search/source filter; installing is
 * escalated to the parent, which owns the tier-appropriate dialog and the resulting job — so this
 * subsection never installs anything itself.
 */
export const BrowseTab: FC<{
	onInstall: (entry: StoreEntry) => void;
	onInstallSpec: () => void;
}> = ({ onInstall, onInstallSpec }) => {
	const catalog = useStoreCatalog();
	// Sources that could not be fetched — the difference between "this host has no plugins" and
	// "the console could not find out".
	const failedSources = (catalog.data?.sources ?? []).filter(
		(src) => src.error || src.stale,
	);
	const [query, setQuery] = useState("");
	const [source, setSource] = useState<string | null>(null);

	const entries = catalog.data?.plugins ?? [];
	const sources = catalog.data?.sources ?? [];
	const shown = useMemo(
		() =>
			entries.filter(
				(e) => (source === null || e.source === source) && matches(e, query),
			),
		[entries, source, query],
	);

	return (
		<div className="flex flex-col gap-card">
			<RunnerBanner />

			<div className="flex flex-col gap-3 sm:flex-row sm:items-center">
				<div className="relative sm:max-w-xs sm:flex-1">
					<Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
					<Input
						type="search"
						className="pl-9"
						aria-label={m.store_search_placeholder()}
						placeholder={m.store_search_placeholder()}
						value={query}
						onChange={(e) => setQuery(e.target.value)}
					/>
				</div>
				{/* One chip per source, so an operator can see a third-party catalog's entries alone. */}
				{sources.length > 1 && (
					<div className="flex flex-wrap gap-2">
						<Button
							size="sm"
							variant={source === null ? "default" : "outline"}
							aria-pressed={source === null}
							onClick={() => setSource(null)}
						>
							{m.store_filter_all()}
						</Button>
						{sources.map((s) => (
							<Button
								key={s.name}
								size="sm"
								variant={source === s.name ? "default" : "outline"}
								aria-pressed={source === s.name}
								onClick={() => setSource(s.name)}
							>
								{s.name}
							</Button>
						))}
					</div>
				)}
			</div>

			<QueryState
				isLoading={catalog.isLoading}
				error={catalog.error}
				refetch={catalog.refetch}
			>
				{shown.length === 0 ? (
					<Card>
						<CardContent
							flush
							className="p-8 text-center text-sm text-muted-foreground"
						>
							{entries.length > 0
								? m.store_no_match()
								: failedSources.length > 0
									? // An all-sources-failed catalog is a SUCCESSFUL request that happens to
										// carry nothing, so "no plugins available" was the console reporting a
										// broken fetch as an empty store. Name the sources that failed.
										m.store_all_sources_failed({
											sources: failedSources.map((f) => f.name).join(", "),
										})
									: m.store_empty()}
						</CardContent>
					</Card>
				) : (
					<div className="@container">
						<div className="grid grid-cols-1 gap-card @xl:grid-cols-2 @4xl:grid-cols-3">
							{shown.map((entry) => (
								<StoreCard
									key={`${entry.source}/${entry.id}`}
									entry={entry}
									onInstall={() => onInstall(entry)}
								/>
							))}
						</div>
					</div>
				)}
			</QueryState>

			{/* The ONLY way to the raw-spec install. Deliberately a quiet footer link, not a button on
			    a card: an unverified install should take a decision, never a stray click. */}
			<div className="text-center">
				<button
					type="button"
					onClick={onInstallSpec}
					className="text-xs text-muted-foreground underline underline-offset-4 transition-colors hover:text-foreground"
				>
					{m.store_spec_open()}
				</button>
			</div>
		</div>
	);
};

/**
 * The catalog stores platform IDENTIFIERS (`linux | windows | macos` — see the index
 * validator); these are their display names. Proper nouns, so deliberately not routed
 * through i18n, and `macos` is spelled the way Apple spells it.
 */
const PLATFORM_LABELS: Record<string, string> = {
	linux: "Linux",
	windows: "Windows",
	macos: "macOS",
};

/**
 * `CardContent` zeroes its top padding (`pt-0`/`sm:pt-0`) because it normally sits under a
 * `CardHeader` that already supplies it — these cards have no header, so the top padding
 * has to come back explicitly, at BOTH breakpoints. `p-card` alone does not do it: `card`
 * is a custom `--spacing-*` token, which tailwind-merge does not recognise as a spacing
 * value and therefore never dedupes against `pt-0`, leaving the longhand to win.
 */
const HEADERLESS_CARD_PADDING = "p-card pt-card sm:pt-card";

/** One catalog entry. Blocked entries shout; incompatible ones grey out; neither can be installed. */
export const StoreCard: FC<{ entry: StoreEntry; onInstall: () => void }> = ({
	entry,
	onInstall,
}) => {
	const Icon = pluginIcon(entry.icon);
	const blocked = entry.blocked !== undefined;
	const installed = entry.installed_version !== undefined;
	const installable = !blocked && entry.compatible;

	return (
		<Card
			className={cn(
				"flex flex-col",
				blocked && "ring-2 ring-destructive/60",
				!entry.compatible && !blocked && "opacity-60",
			)}
		>
			<CardContent
				className={cn("flex flex-1 flex-col gap-3", HEADERLESS_CARD_PADDING)}
			>
				<div className="flex items-start gap-3">
					<span className="flex size-10 shrink-0 items-center justify-center rounded-md bg-primary/15">
						<Icon className="size-5 text-foreground" />
					</span>
					<div className="min-w-0 flex-1">
						<h3 className="truncate font-medium" title={entry.title}>
							{entry.title}
						</h3>
						<p className="truncate text-xs text-muted-foreground">
							{m.store_by_author({ author: entry.author })} · v{entry.version}
						</p>
					</div>
				</div>

				<div className="flex flex-wrap items-center gap-2">
					<TierBadge tier={entry.tier} />
					{/* Attribution, never verification: an external entry names who curated it. */}
					{entry.tier === "external" && <SourceChip source={entry.source} />}
				</div>

				<p className="line-clamp-3 text-sm text-muted-foreground">
					{entry.description}
				</p>

				<div className="flex flex-wrap gap-1.5">
					{entry.platforms.map((p) => (
						<Badge key={p} variant="secondary" className="font-normal">
							{PLATFORM_LABELS[p] ?? p}
						</Badge>
					))}
				</div>

				{blocked && (
					<p className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm font-medium text-destructive">
						<Ban className="mt-0.5 size-4 shrink-0" />
						<span>{m.store_blocked({ reason: entry.blocked ?? "" })}</span>
					</p>
				)}

				{!entry.compatible && !blocked && (
					<p className="flex items-start gap-2 text-xs text-amber-600 dark:text-amber-500">
						<AlertTriangle className="mt-px size-3.5 shrink-0" />
						<span>{entry.incompatible_reason ?? m.store_incompatible()}</span>
					</p>
				)}

				{/* Footer pinned to the bottom so cards in a row line their actions up. */}
				<div className="mt-auto flex items-center gap-3 pt-1">
					{entry.update_available ? (
						<Button size="sm" disabled={!installable} onClick={onInstall}>
							<Download className="size-4" />
							{m.store_update_to({ version: entry.version })}
						</Button>
					) : installed ? (
						<span className="inline-flex items-center gap-1.5 text-sm text-muted-foreground">
							<Check className="size-4" />
							{m.store_installed_label()}
						</span>
					) : (
						<Button size="sm" disabled={!installable} onClick={onInstall}>
							<Download className="size-4" />
							{m.store_install()}
						</Button>
					)}
					{entry.homepage && (
						<a
							href={entry.homepage}
							target="_blank"
							rel="noreferrer"
							className="ml-auto text-xs text-muted-foreground underline underline-offset-4 transition-colors hover:text-foreground"
						>
							{m.store_homepage()}
						</a>
					)}
				</div>
			</CardContent>
		</Card>
	);
};
