// The plugin store: catalog, install/uninstall jobs, catalog sources, and the scripting runner.
// Like `api/plugins.ts` this is a hand-written client rather than an orval-generated one — the
// OpenAPI document is regenerated on a Linux box (punktfunk-host doesn't build on macOS), so the
// console must not wait on a regen to talk to these endpoints. It rides the same `/api` BFF path as
// every other call, so the bearer token is injected server-side and the browser only ever sends its
// session cookie.
import {
	type QueryClient,
	useMutation,
	useQuery,
	useQueryClient,
} from "@tanstack/react-query";
import { apiFetch } from "@/api/fetcher";
import { boostPluginPolling } from "@/api/plugins";

/**
 * How much a plugin's provenance is worth, from most to least trustworthy:
 *
 * - `verified` — from the built-in `unom` source; unom reviewed that exact tarball.
 * - `external` — from an operator-added source; pinned and integrity-checked, but curated by
 *   somebody else. It is NOT reviewed by unom and never wears the verified badge.
 * - `unverified` — installed from a raw package spec through the high-friction dialog. No catalog,
 *   no review, no pinning. Stays badged unverified forever.
 * - `cli` — installed with the CLI, so the host holds no provenance record at all.
 */
export type StoreTier = "verified" | "external" | "unverified" | "cli";

/** The tier a *catalog* entry can carry — the raw-spec/CLI tiers never appear in a catalog. */
export type CatalogTier = Extract<StoreTier, "verified" | "external">;

export interface StoreHostInfo {
	version: string;
	platform: string;
}

export interface StoreSource {
	name: string;
	url: string;
	/** The `unom` source ships with the host: it can't be edited or removed. */
	builtin: boolean;
	signed: boolean;
	/** The last fetch failed or is too old — entries may be out of date. */
	stale: boolean;
	/** Unix seconds of the last successful fetch. */
	fetched_at: number;
	error?: string;
	entry_count: number;
	public_key?: string;
}

export interface StoreEntry {
	id: string;
	pkg: string;
	title: string;
	description: string;
	icon?: string;
	author: string;
	homepage?: string;
	license?: string;
	version: string;
	/** Name of the source this entry came from — the attribution an external entry shows. */
	source: string;
	tier: CatalogTier;
	reviewed_at?: string;
	platforms: string[];
	min_host?: string;
	compatible: boolean;
	incompatible_reason?: string;
	installed_version?: string;
	update_available: boolean;
	blocked?: string;
	/**
	 * What kind of plugin this is. Browse filters on these, and the Library section's "Add a source"
	 * rail shows exactly the `library` ones (design D5/D6). Absent on an index that predates them.
	 */
	categories?: string[];
	/**
	 * Whether the launcher this plugin scans looks installed on this host, from the index's own
	 * existence probes (design D8). `undefined` = the entry declares no probes for this platform,
	 * which is "unknown" and must render differently from "not installed".
	 */
	detected?: boolean;
}

export interface StoreCatalog {
	host: StoreHostInfo;
	sources: StoreSource[];
	plugins: StoreEntry[];
	/** An install/uninstall is already running — the host takes one at a time. */
	busy: boolean;
}

export interface InstalledPlugin {
	pkg: string;
	/** Nullable in the contract (`InstalledView.version`) — a CLI-installed plugin may carry no
	 * recorded version. Typed required here, the Installed tab rendered the literal "vundefined". */
	version?: string | null;
	tier: StoreTier;
	source?: string;
	entry_id?: string;
	/** The plugin's runtime id — how it's keyed in the plugin directory (`api/plugins.ts`). */
	plugin_id?: string;
	title?: string;
	installed_at?: string;
	running: boolean;
	/** The version available to update to, if any. */
	update_available?: string;
	blocked?: string;
}

/** An installed plugin paired with the catalog entry an update would install. */
export interface PendingUpdate {
	plugin: InstalledPlugin;
	entry: StoreEntry;
}

/** What "Update all" would do: the run, and what it deliberately left out of it. */
export interface UpdatePlan {
	/** Ready to install, in the order the run will work through them. */
	updates: PendingUpdate[];
	/** Display names of plugins offering an update this host cannot take right now. */
	skipped: string[];
}

/**
 * The catalog entry an installed plugin updates FROM.
 *
 * Resolve by the entry the plugin was actually installed from (source + entry id) before falling
 * back to the package name: two sources may carry the same `pkg`, and matching on the name alone
 * could offer a row badged "verified" an entry from somebody else's source at a different version.
 */
export function catalogEntryFor(
	plugin: InstalledPlugin,
	entries: StoreEntry[] | undefined,
): StoreEntry | undefined {
	const list = entries ?? [];
	return (
		(plugin.source && plugin.entry_id
			? list.find((e) => e.source === plugin.source && e.id === plugin.entry_id)
			: undefined) ??
		(plugin.source
			? list.find((e) => e.source === plugin.source && e.pkg === plugin.pkg)
			: undefined) ??
		list.find((e) => e.pkg === plugin.pkg)
	);
}

/**
 * Everything an "Update all" run would install, in the installed list's own order — so the run
 * follows the rows on screen rather than some order of its own.
 *
 * Two things are dropped rather than attempted, and both are reported instead of hidden: an update
 * with no catalog entry to install (the same dead end a single row's button reports on click), and
 * an entry this host will refuse — incompatible ones are a `400` from `POST /store/install`, and a
 * blocked one is what Browse already greys its Install button out for. Either would end the run on
 * a failure card that says nothing about the updates still queued behind it, so they never enter
 * the queue in the first place.
 *
 * `plugin.blocked` is NOT a reason to skip: that advisory is against the version installed right
 * now, and updating away from it is the fix, not the risk.
 */
export function planUpdates(
	installed: InstalledPlugin[] | undefined,
	entries: StoreEntry[] | undefined,
): UpdatePlan {
	const plan: UpdatePlan = { updates: [], skipped: [] };
	for (const plugin of installed ?? []) {
		if (plugin.update_available === undefined) continue;
		const entry = catalogEntryFor(plugin, entries);
		if (entry?.compatible && entry.blocked === undefined) {
			plan.updates.push({ plugin, entry });
		} else {
			plan.skipped.push(plugin.title ?? plugin.pkg);
		}
	}
	return plan;
}

export type JobKind = "install" | "uninstall";
export type JobState = "running" | "done" | "failed";

export interface StoreJob {
	id: string;
	kind: JobKind;
	target: string;
	state: JobState;
	phase: string;
	log: string[];
	error?: string;
	started_at: number;
	finished_at?: number;
}

export interface RuntimeStatus {
	installed: boolean;
	enabled: boolean;
	running: boolean;
	unit: string;
	principal?: string;
	detail?: string;
}

/** What `POST /store/install` and `POST /store/uninstall` answer with (202). */
export interface JobAccepted {
	job: string;
}

/**
 * Install a curated catalog entry, or — deliberately awkward — a raw package spec.
 *
 * The raw-spec branch carries the console `password`: it runs unreviewed code, so the BFF
 * re-confirms it (server/routes/api/v1/store/install.post.ts) and strips it before the host ever
 * sees the request. A catalog install needs no password — that trust decision was made when the
 * source was added.
 */
export type InstallBody =
	| { source: string; id: string }
	| { spec: string; accept_unverified: true; password: string };

/** Adding or repointing a source is a trust-root change, so it carries the console password too
 * (stripped at the BFF — server/routes/api/v1/store/sources/[name].put.ts). */
export interface SourceBody {
	url: string;
	public_key?: string;
	password: string;
}

const BASE = "/api/v1/store";

/** Query keys, in one place so any mutation can invalidate precisely. */
export const storeKeys = {
	all: ["store"] as const,
	catalog: ["store", "catalog"] as const,
	installed: ["store", "installed"] as const,
	sources: ["store", "sources"] as const,
	runtime: ["store", "runtime"] as const,
	job: (id: string) => ["store", "job", id] as const,
};

const json = (method: string, body: unknown): RequestInit => ({
	method,
	headers: { "Content-Type": "application/json" },
	body: JSON.stringify(body),
});

/**
 * Refresh everything a completed install/uninstall touches — the catalog (installed markers), the
 * installed list, the runner state (it restarts), and the plugin directory the nav is built from.
 */
export function invalidateStore(qc: QueryClient): Promise<void> {
	// The runner restarts AFTER the job reports done, so the plugin registers its UI a few seconds
	// from now — this invalidation would otherwise refetch the pre-install list and stop looking.
	boostPluginPolling();
	return Promise.all([
		qc.invalidateQueries({ queryKey: storeKeys.catalog }),
		qc.invalidateQueries({ queryKey: storeKeys.installed }),
		qc.invalidateQueries({ queryKey: storeKeys.sources }),
		qc.invalidateQueries({ queryKey: storeKeys.runtime }),
		qc.invalidateQueries({ queryKey: ["plugins"] }),
	]).then(() => undefined);
}

/** The merged catalog across every source, plus the sources' own health. */
export function useStoreCatalog() {
	return useQuery({
		queryKey: storeKeys.catalog,
		queryFn: () => apiFetch<StoreCatalog>(`${BASE}/catalog`),
	});
}

/** What's installed right now, with each plugin's permanent provenance tier. */
export function useInstalledPlugins() {
	return useQuery({
		queryKey: storeKeys.installed,
		queryFn: () => apiFetch<InstalledPlugin[]>(`${BASE}/installed`),
		refetchInterval: 30_000,
	});
}

export function useStoreSources() {
	return useQuery({
		queryKey: storeKeys.sources,
		queryFn: () => apiFetch<StoreSource[]>(`${BASE}/sources`),
	});
}

export function useStoreRuntime() {
	return useQuery({
		queryKey: storeKeys.runtime,
		queryFn: () => apiFetch<RuntimeStatus>(`${BASE}/runtime`),
	});
}

/**
 * A single install/uninstall job, polled once a second while it runs and left alone once it
 * settles. Pass `null` to park the query (no job in flight).
 *
 * The interval keys off "not finished yet" rather than off `state === "running"`. `data` is
 * undefined in two live cases — the first poll has not landed, and the first poll FAILED — and
 * treating those as "stop polling" wedged the card: an install whose very first poll lost the race
 * with a busy host never polled again and the operator saw nothing at all, while an install that
 * restarts the runner (every successful one does) could drop a poll mid-flight.
 *
 * The failure count bounds it: jobs live in host memory, so a host restart makes the id 404 for
 * good, and something has to stop asking.
 */
const JOB_POLL_MS = 1_000;
const JOB_MAX_FAILURES = 15;

/**
 * The host's recent jobs, used to RE-ATTACH after a reload.
 *
 * The in-flight job id lived only in component state, so reloading the page (or opening the console
 * on another device) lost all trace of a running install while the Install buttons stayed armed —
 * and the host takes one job at a time, so the next click just bounced off a 409. The host keeps
 * the list; ask it rather than remembering.
 */
export function useStoreJobs() {
	return useQuery({
		queryKey: [...storeKeys.all, "jobs"] as const,
		queryFn: () => apiFetch<StoreJob[]>(`${BASE}/jobs`),
		// Only needed to find an orphaned job on mount; the job query itself does the live polling.
		staleTime: 5_000,
	});
}

/** The newest job that is still running, if any — what a fresh page should re-attach to. */
export function runningJob(jobs: StoreJob[] | undefined): StoreJob | undefined {
	if (!jobs) return undefined;
	// The list is oldest-first, so scan from the end for the most recent live one.
	for (let i = jobs.length - 1; i >= 0; i--) {
		const j = jobs[i];
		if (j?.state === "running") return j;
	}
	return undefined;
}

export function useStoreJob(id: string | null) {
	return useQuery({
		queryKey: storeKeys.job(id ?? ""),
		queryFn: () =>
			apiFetch<StoreJob>(`${BASE}/jobs/${encodeURIComponent(id ?? "")}`),
		enabled: id !== null,
		refetchInterval: (q) => {
			const state = q.state.data?.state;
			if (state === "done" || state === "failed") return false;
			if (q.state.fetchFailureCount > JOB_MAX_FAILURES) return false;
			return JOB_POLL_MS;
		},
		// A job that vanished with its host is gone for good; a transient blip is not. Retry a few
		// times per poll so a runner restart doesn't surface as an error card.
		retry: 3,
	});
}

/** Re-fetch every source's index; answers with the freshly merged catalog. */
export function useRefreshCatalog() {
	const qc = useQueryClient();
	return useMutation({
		mutationFn: () =>
			apiFetch<StoreCatalog>(`${BASE}/refresh`, { method: "POST" }),
		onSuccess: (catalog) => {
			qc.setQueryData(storeKeys.catalog, catalog);
			qc.setQueryData(storeKeys.sources, catalog.sources);
		},
	});
}

/** Start an install. Answers 202 with the job to poll; 409 means another op is already running. */
export function useInstallPlugin() {
	return useMutation({
		mutationFn: (body: InstallBody) =>
			apiFetch<JobAccepted>(`${BASE}/install`, json("POST", body)),
	});
}

export function useUninstallPlugin() {
	return useMutation({
		mutationFn: (pkg: string) =>
			apiFetch<JobAccepted>(`${BASE}/uninstall`, json("POST", { pkg })),
	});
}

/** Add or update an operator source (the built-in one is not editable). */
export function useSetSource() {
	const qc = useQueryClient();
	return useMutation({
		mutationFn: ({ name, ...body }: SourceBody & { name: string }) =>
			apiFetch<void>(
				`${BASE}/sources/${encodeURIComponent(name)}`,
				json("PUT", body),
			),
		onSuccess: () => {
			qc.invalidateQueries({ queryKey: storeKeys.sources });
			qc.invalidateQueries({ queryKey: storeKeys.catalog });
		},
	});
}

export function useDeleteSource() {
	const qc = useQueryClient();
	return useMutation({
		mutationFn: (name: string) =>
			apiFetch<void>(`${BASE}/sources/${encodeURIComponent(name)}`, {
				method: "DELETE",
			}),
		onSuccess: () => {
			qc.invalidateQueries({ queryKey: storeKeys.sources });
			qc.invalidateQueries({ queryKey: storeKeys.catalog });
		},
	});
}

/** Enable or disable the plugin/script runner service. */
export function useSetRuntime() {
	const qc = useQueryClient();
	return useMutation({
		mutationFn: (enabled: boolean) =>
			apiFetch<RuntimeStatus>(`${BASE}/runtime`, json("POST", { enabled })),
		onSuccess: (status) => {
			qc.setQueryData(storeKeys.runtime, status);
			qc.invalidateQueries({ queryKey: ["plugins"] });
		},
	});
}
