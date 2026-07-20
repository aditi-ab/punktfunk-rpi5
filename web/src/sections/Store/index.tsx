import Section from "@unom/ui/section";
import { toast } from "@unom/ui/toast";
import { type FC, useState } from "react";
import { ApiError } from "@/api/fetcher";
import {
	type InstallBody,
	type InstalledPlugin,
	type StoreEntry,
	useInstallPlugin,
	useStoreCatalog,
	useUninstallPlugin,
} from "@/api/store";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { BrowseTab } from "./Browse";
import { InstallDialog, SpecInstallDialog } from "./InstallDialogs";
import { InstalledTab } from "./Installed";
import { JobProgressSection } from "./JobProgress";
import { SourcesTab } from "./Sources";

type StoreTab = "browse" | "installed" | "sources";

/**
 * The plugin store: browse a catalog, manage what's installed, and choose which catalogs this host
 * trusts. Each tab owns its own queries; this container owns only what genuinely spans them — the
 * install/uninstall mutations, the confirm dialogs their trust tier dictates, and the job the host
 * hands back (which must stay visible whichever tab you switch to while it runs).
 */
export const SectionStore: FC = () => {
	useLocale();
	const [tab, setTab] = useState<StoreTab>("browse");
	// The catalog entry awaiting its install confirmation, and the raw-spec dialog's open state.
	const [target, setTarget] = useState<StoreEntry | null>(null);
	const [specOpen, setSpecOpen] = useState(false);
	// The job the host is running for us, if any. Cleared by the operator, not by completion — a
	// finished job's log is the only record of what happened.
	const [jobId, setJobId] = useState<string | null>(null);

	const catalog = useStoreCatalog();
	const install = useInstallPlugin();
	const uninstall = useUninstallPlugin();

	/** Turn a failed 202-request into a message: 409 means the host is busy, not that we're broken. */
	const failed = (e: unknown, fallback: string) =>
		toast.error(
			e instanceof ApiError && e.status === 409 ? m.store_busy() : fallback,
		);

	const start = async (body: InstallBody) => {
		try {
			const { job } = await install.mutateAsync(body);
			setJobId(job);
		} catch (e) {
			failed(e, m.store_install_failed());
		}
	};

	const onConfirmEntry = async (entry: StoreEntry) => {
		setTarget(null);
		await start({ source: entry.source, id: entry.id });
	};

	const onConfirmSpec = async (spec: string) => {
		setSpecOpen(false);
		await start({ spec, accept_unverified: true });
	};

	// An update from the Installed tab installs the CATALOG version — so it goes through the very
	// same tier-appropriate dialog a fresh install would, warning included.
	const onUpdate = (plugin: InstalledPlugin) => {
		const entry = catalog.data?.plugins.find((e) => e.pkg === plugin.pkg);
		if (!entry) {
			toast.error(m.store_update_no_entry());
			return;
		}
		setTarget(entry);
	};

	const onUninstall = async (plugin: InstalledPlugin) => {
		if (
			!confirm(m.store_uninstall_confirm({ title: plugin.title ?? plugin.pkg }))
		)
			return;
		try {
			const { job } = await uninstall.mutateAsync(plugin.pkg);
			setJobId(job);
		} catch (e) {
			failed(e, m.store_uninstall_failed());
		}
	};

	return (
		<Section maxWidth={false}>
			<div className="flex flex-col gap-card">
				<div className="space-y-1">
					<h1 className="text-2xl font-semibold">{m.store_title()}</h1>
					<p className="text-sm text-muted-foreground">{m.store_subtitle()}</p>
				</div>

				{jobId && (
					<JobProgressSection jobId={jobId} onDismiss={() => setJobId(null)} />
				)}

				<Tabs value={tab} onValueChange={(v) => setTab(v as StoreTab)}>
					<TabsList>
						<TabsTrigger value="browse">{m.store_tab_browse()}</TabsTrigger>
						<TabsTrigger value="installed">
							{m.store_tab_installed()}
						</TabsTrigger>
						<TabsTrigger value="sources">{m.store_tab_sources()}</TabsTrigger>
					</TabsList>

					<TabsContent value="browse">
						<BrowseTab
							onInstall={setTarget}
							onInstallSpec={() => setSpecOpen(true)}
						/>
					</TabsContent>
					<TabsContent value="installed">
						<InstalledTab
							onUpdate={onUpdate}
							onUninstall={onUninstall}
							busyPkg={
								uninstall.isPending ? (uninstall.variables ?? null) : null
							}
						/>
					</TabsContent>
					<TabsContent value="sources">
						<SourcesTab />
					</TabsContent>
				</Tabs>

				<InstallDialog
					entry={target}
					isPending={install.isPending}
					onCancel={() => setTarget(null)}
					onConfirm={onConfirmEntry}
				/>
				<SpecInstallDialog
					open={specOpen}
					isPending={install.isPending}
					onCancel={() => setSpecOpen(false)}
					onConfirm={onConfirmSpec}
				/>
			</div>
		</Section>
	);
};
