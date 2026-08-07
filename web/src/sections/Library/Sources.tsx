import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Check, Download, Settings2, Trash2 } from "lucide-react";
import { type FC, useState } from "react";
import {
	getGetLibraryQueryKey,
	getListLibraryScannersQueryKey,
	useDeleteProviderEntries,
	useListLibraryScanners,
	useSetLibraryScanner,
} from "@/api/gen/library/library";
import type { ScannerInfo } from "@/api/gen/model/scannerInfo";
import { libraryPlugins, usePlugins } from "@/api/plugins";
import {
	type StoreEntry,
	useInstallPlugin,
	useStoreCatalog,
} from "@/api/store";
import { useDialogs } from "@/components/dialogs";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";
import { SourceSettingsDialog } from "./SourceSettings";

/**
 * **Game sources** — the single surface for "where do my games come from", merging what used to be
 * two cards (the scanner toggles and the "synced by plugins" list).
 *
 * They were split because they were different things: scanners were compiled into the host and
 * plugins were an afterthought. After the extraction they are the *same* thing — the host reports
 * one list of sources whose ids match whether they came from a built-in scanner or the plugin that
 * replaced it — so one surface is both simpler and the only honest presentation (design D6).
 *
 * Deliberately kept under the existing "Game sources" label rather than a new "Plugins" heading:
 * `store_title` and `nav_plugins` are both already "Plugins", and a third would be worse than the
 * merge is good.
 */
export const SourcesSection: FC<{
	/** The provider currently filtered to in the grid, or null for "everything". */
	activeFilter: string | null;
	onFilter: (provider: string | null) => void;
}> = ({ activeFilter, onFilter }) => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const scanners = useListLibraryScanners();
	const toggle = useSetLibraryScanner();
	const purge = useDeleteProviderEntries();
	const plugins = usePlugins();
	const catalog = useStoreCatalog();
	const install = useInstallPlugin();
	const [settingsFor, setSettingsFor] = useState<ScannerInfo | null>(null);

	const onToggle = async (source: ScannerInfo) => {
		try {
			// The PUT answers with the full updated list — seed the query cache with it directly,
			// then refetch the library so the grid reflects the new source set.
			const list = await toggle.mutateAsync({
				id: source.id,
				data: { enabled: !source.enabled },
			});
			qc.setQueryData(getListLibraryScannersQueryKey(), list);
			await qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });
		} catch {
			toast.error(m.library_sources_failed());
		}
	};

	const onPurge = async (source: ScannerInfo) => {
		const provider = source.provider ?? source.id;
		const count = source.entries ?? 0;
		const ok = await confirm({
			title: m.library_provider_purge_confirm({ provider, count }),
			description: m.library_provider_purge_body(),
			confirmLabel: m.common_remove(),
			destructive: true,
		});
		if (!ok) return;
		try {
			await purge.mutateAsync({ provider });
			qc.invalidateQueries({ queryKey: getGetLibraryQueryKey() });
			qc.invalidateQueries({ queryKey: getListLibraryScannersQueryKey() });
			if (activeFilter === provider) onFilter(null);
			toast.success(m.library_provider_purged({ provider }));
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_provider_purge_failed());
		}
	};

	const onInstall = async (entry: StoreEntry) => {
		try {
			// Install by (source, id) — the catalogued, integrity-pinned path. The raw-spec form is
			// for unverified installs and must never be reachable from a one-click rail.
			await install.mutateAsync({ source: entry.source, id: entry.id });
			toast.success(m.library_source_installing({ title: entry.title }));
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.library_source_install_failed());
		}
	};

	// This is a secondary control: when the API is down the grid's own QueryState already tells the
	// story, so render nothing rather than a second error banner.
	if (!scanners.data) return null;

	// Catalog rows that are library sources and not already installed — the "Add a source" rail.
	const installedPkgs = new Set(
		(catalog.data?.plugins ?? [])
			.filter((p) => p.installed_version)
			.map((p) => p.pkg),
	);
	const available = (catalog.data?.plugins ?? []).filter(
		(p) => p.categories?.includes("library") && !installedPkgs.has(p.pkg),
	);
	const running = new Set(libraryPlugins(plugins.data).map((p) => p.id));

	// The bridge-release nudge (design D9): a built-in scanner still doing the work, with its
	// replacement plugin sitting uninstalled in the catalog. One click per scanner, and NEVER a
	// silent auto-install — installing code stays an explicit operator act.
	const migratable = scanners.data
		.filter((s) => s.origin === "builtin" && s.enabled)
		.map((s) => ({
			source: s,
			entry: available.find((p) => p.id === s.id && p.compatible),
		}))
		.filter((r): r is { source: ScannerInfo; entry: StoreEntry } => !!r.entry);

	return (
		<>
			{migratable.length > 0 && (
				<MigrationBanner
					rows={migratable}
					busy={catalog.data?.busy === true || install.isPending}
					onInstall={onInstall}
				/>
			)}
			<SourcesCard
				sources={scanners.data}
				available={available}
				running={running}
				busyId={toggle.isPending ? (toggle.variables?.id ?? null) : null}
				installBusy={catalog.data?.busy === true || install.isPending}
				activeFilter={activeFilter}
				onToggle={onToggle}
				onFilter={onFilter}
				onSettings={setSettingsFor}
				onPurge={onPurge}
				onInstall={onInstall}
			/>
			{settingsFor && (
				<SourceSettingsDialog
					source={settingsFor}
					onClose={() => setSettingsFor(null)}
				/>
			)}
		</>
	);
};

/**
 * "Game sources are moving to plugins" — shown only while a built-in scanner is still doing a job a
 * catalogued plugin could take over.
 *
 * One button per scanner rather than a single "migrate everything": installing a plugin is an
 * explicit operator act under the store's consent model, and per-scanner is also what makes it safe
 * to repeat — the claim suppresses the built-in idempotently (design D2), so a half-finished
 * migration is a valid state rather than a mess.
 */
export const MigrationBanner: FC<{
	rows: ReadonlyArray<{ source: ScannerInfo; entry: StoreEntry }>;
	busy: boolean;
	onInstall: (entry: StoreEntry) => void;
}> = ({ rows, busy, onInstall }) => (
	<Card>
		<CardHeader className="pb-3">
			<CardTitle className="text-base">{m.library_migrate_title()}</CardTitle>
		</CardHeader>
		<CardContent className="space-y-3">
			<p className="max-w-prose text-sm text-muted-foreground">
				{m.library_migrate_help()}
			</p>
			<div className="flex flex-wrap gap-2">
				{rows.map(({ source, entry }) => (
					<Button
						key={source.id}
						size="sm"
						variant="outline"
						disabled={busy}
						onClick={() => onInstall(entry)}
					>
						<Download className="size-4" />
						{m.library_migrate_install({ source: source.label })}
					</Button>
				))}
			</div>
		</CardContent>
	</Card>
);

/** The sources card itself — presentational, so Storybook can drive every state. */
export const SourcesCard: FC<{
	sources: ScannerInfo[];
	/** Catalog rows offering a library source that isn't installed yet. */
	available: StoreEntry[];
	/** Ids of library plugins whose lease is currently live. */
	running: Set<string>;
	/** Source id whose toggle is in flight, or null — only that row disables. */
	busyId: string | null;
	installBusy: boolean;
	activeFilter: string | null;
	onToggle: (source: ScannerInfo) => void;
	onFilter: (provider: string | null) => void;
	onSettings: (source: ScannerInfo) => void;
	onPurge: (source: ScannerInfo) => void;
	onInstall: (entry: StoreEntry) => void;
}> = ({
	sources,
	available,
	running,
	busyId,
	installBusy,
	activeFilter,
	onToggle,
	onFilter,
	onSettings,
	onPurge,
	onInstall,
}) => (
	<Card>
		<CardHeader className="pb-3">
			<CardTitle className="text-base">{m.library_sources_title()}</CardTitle>
		</CardHeader>
		<CardContent className="space-y-4">
			<div className="flex flex-col gap-2">
				{sources.map((source) => (
					<SourceRow
						key={source.id}
						source={source}
						running={running.has(source.id)}
						busy={busyId === source.id}
						filtered={
							activeFilter !== null &&
							activeFilter === (source.provider ?? source.id)
						}
						onToggle={() => onToggle(source)}
						onFilter={() => {
							const p = source.provider ?? source.id;
							onFilter(activeFilter === p ? null : p);
						}}
						onSettings={() => onSettings(source)}
						onPurge={() => onPurge(source)}
					/>
				))}
			</div>
			<p className="max-w-prose text-xs text-muted-foreground">
				{m.library_sources_help()}
			</p>

			{available.length > 0 && (
				<div className="space-y-2 border-t pt-4">
					<p className="text-sm font-medium">{m.library_add_source()}</p>
					<div className="flex flex-wrap gap-2">
						{available.map((entry) => (
							<Button
								key={entry.pkg}
								size="sm"
								variant="outline"
								disabled={!entry.compatible || installBusy}
								title={entry.incompatible_reason ?? entry.description}
								onClick={() => onInstall(entry)}
							>
								<Download className="size-4" />
								{entry.title}
								{/* `detected` is tri-state: only badge a POSITIVE probe. An entry with
								    no probes for this platform is "unknown", and labelling that "not
								    installed" would be a lie. */}
								{entry.detected === true && (
									<Badge variant="secondary">
										{m.library_source_detected()}
									</Badge>
								)}
							</Button>
						))}
					</div>
				</div>
			)}
		</CardContent>
	</Card>
);

/** One source row: enable toggle, provenance, counts, and its per-source actions. */
const SourceRow: FC<{
	source: ScannerInfo;
	/** The plugin backing this source is currently registered (its lease is live). */
	running: boolean;
	busy: boolean;
	filtered: boolean;
	onToggle: () => void;
	onFilter: () => void;
	onSettings: () => void;
	onPurge: () => void;
}> = ({
	source,
	running,
	busy,
	filtered,
	onToggle,
	onFilter,
	onSettings,
	onPurge,
}) => {
	const isPlugin = source.origin === "plugin";
	return (
		<div className="flex flex-wrap items-center gap-3 rounded-lg border p-3">
			<Button
				size="sm"
				variant={source.enabled ? "default" : "outline"}
				aria-pressed={source.enabled}
				disabled={busy}
				onClick={onToggle}
			>
				{source.enabled && <Check className="size-4" />}
				{source.label}
			</Button>
			{isPlugin && (
				<Badge variant={running ? "secondary" : "outline"}>
					{running ? m.library_source_running() : m.library_source_stopped()}
				</Badge>
			)}
			{source.entries != null && (
				<Badge variant="secondary">
					{m.library_provider_count({ count: source.entries })}
				</Badge>
			)}
			<div className="ml-auto flex gap-2">
				{isPlugin && (
					<>
						<Button
							size="sm"
							variant={filtered ? "default" : "outline"}
							aria-pressed={filtered}
							onClick={onFilter}
						>
							{filtered
								? m.library_provider_show_all()
								: m.library_provider_filter()}
						</Button>
						<Button
							size="sm"
							variant="outline"
							aria-label={m.library_source_settings()}
							onClick={onSettings}
						>
							<Settings2 className="size-4" />
						</Button>
						<Button
							size="sm"
							variant="outline"
							aria-label={m.library_provider_purge()}
							onClick={onPurge}
						>
							<Trash2 className="size-4 text-destructive" />
						</Button>
					</>
				)}
			</div>
		</div>
	);
};
