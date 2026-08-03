import { toast } from "@unom/ui/toast";
import { Copy, Download, Pause, Play, Share2, Trash2 } from "lucide-react";
import { type FC, useEffect, useMemo, useRef, useState } from "react";
import { useLogsGet } from "@/api/gen/logs/logs";
import type { LogEntry } from "@/api/gen/model/logEntry";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import {
	detectShareMode,
	downloadText,
	logFilename,
	logsToText,
	type ShareMode,
	shareLogs,
} from "./export";

const LEVELS = ["DEBUG", "INFO", "WARN", "ERROR"] as const;
type MinLevel = (typeof LEVELS)[number];
const RANK: Record<string, number> = {
	TRACE: 0,
	DEBUG: 1,
	INFO: 2,
	WARN: 3,
	ERROR: 4,
};
const LEVEL_CLASS: Record<string, string> = {
	ERROR: "text-red-400",
	WARN: "text-amber-400",
	INFO: "text-sky-300",
	DEBUG: "text-muted-foreground",
	TRACE: "text-muted-foreground",
};

const KEEP = 5_000; // accumulated entries (client memory bound)
const SHOW = 1_000; // rendered rows (DOM bound)

/**
 * Producer filter. The ring carries the host's own `tracing` events AND whatever the plugin runner
 * ships up (`POST /api/v1/plugins/logs`), the latter targeted `plugin:<name>`. Without this the two
 * are interleaved with nothing but the target column to tell them apart, and "show me what my
 * plugin said" — the question that sends people to `journalctl` — means knowing to type `plugin:`
 * into the search box.
 */
const SOURCES = ["all", "host", "plugins"] as const;
type Source = (typeof SOURCES)[number];

/** The target prefix the host stamps on every runner-shipped line. */
const PLUGIN_TARGET_PREFIX = "plugin:";

const matchesSource = (target: string, source: Source): boolean =>
	source === "all" ||
	(source === "plugins") === target.startsWith(PLUGIN_TARGET_PREFIX);

const SOURCE_LABEL: Record<Source, () => string> = {
	all: () => m.logs_source_all(),
	host: () => m.logs_source_host(),
	plugins: () => m.logs_source_plugins(),
};

/**
 * Container: cursor-paged log polling. A non-empty page advances the cursor — a new query key,
 * so the next page fetches immediately and a backlog drains fast; an empty page leaves the key
 * unchanged and `refetchInterval` paces the idle poll. Pausing (follow off) stops the interval.
 */
export const LogsSection: FC = () => {
	const [cursor, setCursor] = useState(0);
	const [entries, setEntries] = useState<LogEntry[]>([]);
	const [follow, setFollow] = useState(true);
	const [dropped, setDropped] = useState(false);
	const [shareMode, setShareMode] = useState<ShareMode | null>(null);
	// Set while a poll has failed and we have not yet re-read the ring from the start.
	const [resync, setResync] = useState(false);

	// Probed after mount: the server render has no `navigator`, and guessing there would mismatch
	// on hydration. Until then the share button is simply absent.
	useEffect(() => {
		setShareMode(detectShareMode());
	}, []);

	const query = useLogsGet(
		{ after: cursor > 0 ? cursor : undefined },
		{
			query: {
				refetchInterval: follow ? 2_000 : false,
				// Pausing must actually pause. Stopping only the interval left React Query's default
				// focus/reconnect refetches landing, and the append effect consumed them
				// unconditionally — so tabbing away and back evicted the lines the operator had
				// paused on, from behind the pause button.
				refetchOnWindowFocus: follow,
				refetchOnReconnect: follow,
			},
		},
	);

	// Resync after the host goes away and comes back.
	//
	// The host's log ring restarts at seq 1 on every restart, while our cursor stays wherever it
	// got to. `GET /logs?after=8000` against a fresh ring is not an error — it is a permanently
	// EMPTY page (`next` echoes `after`), so the page would poll forever showing stale lines with
	// no error, no dropped badge and no way back short of a full reload. The console's own update
	// flow restarts the host, so this was reachable from two clicks away.
	//
	// A restart always breaks the poll first, so a failed query is the trigger: on the next success
	// we re-read from the start of the ring once and let the effect below decide whether the
	// sequence actually regressed.
	const failed = query.isError;
	useEffect(() => {
		if (failed) setResync(true);
	}, [failed]);
	useEffect(() => {
		if (resync && cursor !== 0) setCursor(0);
	}, [resync, cursor]);

	const data = query.data;
	useEffect(() => {
		if (!data || data.entries.length === 0) return;
		setEntries((prev) => {
			const lastSeq = prev.at(-1)?.seq ?? -1;
			// A page whose newest entry is OLDER than what we already hold can only mean the host's
			// sequence restarted underneath us — the buffer describes a host that no longer exists,
			// so replace it wholesale rather than filtering every new line away as "already seen".
			const newest = data.entries.at(-1)?.seq ?? -1;
			if (newest < lastSeq) return data.entries.slice(-KEEP);
			// Otherwise append only what's newer — dedup by the monotonic `seq`. Guards a
			// double-invoked mount effect (React StrictMode, or `data` warm in cache) from appending
			// the same page twice (duplicate rows + duplicate React keys), and makes the post-resync
			// re-read from 0 a no-op when the host did NOT restart.
			const fresh = data.entries.filter((e) => e.seq > lastSeq);
			return fresh.length ? [...prev, ...fresh].slice(-KEEP) : prev;
		});
		setDropped((d) => d || data.dropped);
		setCursor(data.next);
		setResync(false);
	}, [data]);

	// The card hands back the entries its filters currently match, so an export carries exactly what
	// the viewer shows — never the DOM-bounded tail of it.
	return (
		<LogsCard
			entries={entries}
			follow={follow}
			onFollow={setFollow}
			onClear={() => {
				setEntries([]);
				setDropped(false);
			}}
			onDownload={(shown) =>
				downloadText(logsToText(shown), logFilename(new Date()))
			}
			onShare={async (shown) => {
				const outcome = await shareLogs(
					logsToText(shown),
					logFilename(new Date()),
				);
				if (outcome === "copied") toast.success(m.logs_copied());
				else if (outcome === "failed") toast.error(m.logs_share_failed());
			}}
			shareMode={shareMode}
			dropped={dropped}
			error={query.error}
			isLoading={query.isLoading}
			onRetry={() => query.refetch()}
		/>
	);
};

/**
 * Pure log viewer: level/min filter + text search (local UI state), follow, clear, and export.
 * Export is the filters' full result, not the rendered tail — the `SHOW` cap is a DOM budget and
 * has no business truncating a file destined for a bug report.
 */
export const LogsCard: FC<{
	entries: LogEntry[];
	follow: boolean;
	onFollow: (follow: boolean) => void;
	onClear: () => void;
	onDownload: (shown: LogEntry[]) => void;
	onShare: (shown: LogEntry[]) => void;
	shareMode: ShareMode | null;
	dropped: boolean;
	/** The poll's failure, if any — without it a broken /logs is indistinguishable from a quiet host. */
	error?: unknown;
	isLoading?: boolean;
	onRetry?: () => void;
}> = ({
	entries,
	follow,
	onFollow,
	onClear,
	onDownload,
	onShare,
	shareMode,
	dropped,
	error,
	isLoading,
	onRetry,
}) => {
	const [minLevel, setMinLevel] = useState<MinLevel>("DEBUG");
	const [source, setSource] = useState<Source>("all");
	const [search, setSearch] = useState("");
	const listRef = useRef<HTMLDivElement>(null);

	const matched = useMemo(() => {
		const min = RANK[minLevel] ?? 0;
		const q = search.trim().toLowerCase();
		return entries.filter(
			(e) =>
				(RANK[e.level] ?? 0) >= min &&
				matchesSource(e.target, source) &&
				(q === "" ||
					e.msg.toLowerCase().includes(q) ||
					e.target.toLowerCase().includes(q)),
		);
	}, [entries, minLevel, source, search]);
	const visible = useMemo(() => matched.slice(-SHOW), [matched]);
	const shareLabel = shareMode === "share" ? m.logs_share() : m.logs_copy();

	// Keep the tail in view while following.
	//
	// Keyed on the newest RENDERED seq, not on `visible.length`: `visible` is `matched.slice(-SHOW)`,
	// so once the filter matches SHOW rows its length is pinned at SHOW forever. The effect then
	// stopped re-running and follow-mode quietly stopped following — exactly when the log is busy
	// enough to need it. The newest seq keeps changing for as long as lines arrive.
	const newestVisible = visible.at(-1)?.seq ?? -1;
	// NOTE: biome flags `newestVisible` as an unnecessary dependency (it is not read in the body) and
	// offers to remove it. Do NOT take that fix — it is a TRIGGER, the signal that new lines arrived.
	// Removing it reinstates the bug this replaced: the effect stops re-running and follow-mode
	// quietly stops following. The same warning was here before, on `visible.length`.
	useEffect(() => {
		if (!follow) return;
		const el = listRef.current;
		if (el) el.scrollTop = el.scrollHeight;
	}, [follow, newestVisible]);

	return (
		<Card>
			{/* This card has no CardHeader, so it has to put the top padding back itself — and it
			    must do so at BOTH breakpoints. `CardContent` is `p-4 pt-0 sm:p-6 sm:pt-0`, and
			    tailwind-merge only resolves conflicts within the same variant: a bare `pt-6` cancels
			    `pt-0` but leaves `sm:pt-0` standing, so the padding was 24px on a phone and 0 on a
			    desktop, with the filter row touching the card's edge. (Same trap the `p-0` note in
			    components/ui/card.tsx describes, in the other direction.) */}
			<CardContent className="flex flex-col gap-3 pt-4 sm:pt-6">
				<div className="flex flex-wrap items-center gap-2">
					<div className="flex items-center gap-1">
						{LEVELS.map((l) => (
							<Button
								key={l}
								size="sm"
								variant={minLevel === l ? "secondary" : "ghost"}
								onClick={() => setMinLevel(l)}
							>
								{l}
							</Button>
						))}
					</div>
					<div className="flex items-center gap-1 border-l pl-2">
						{SOURCES.map((s) => (
							<Button
								key={s}
								size="sm"
								variant={source === s ? "secondary" : "ghost"}
								onClick={() => setSource(s)}
							>
								{SOURCE_LABEL[s]()}
							</Button>
						))}
					</div>
					<Input
						value={search}
						onChange={(e) => setSearch(e.target.value)}
						placeholder={m.logs_search()}
						className="max-w-xs"
					/>
					<div className="ml-auto flex items-center gap-2">
						{dropped && <Badge variant="secondary">{m.logs_dropped()}</Badge>}
						<Button
							size="icon"
							variant="ghost"
							disabled={matched.length === 0}
							title={m.logs_download()}
							aria-label={m.logs_download()}
							onClick={() => onDownload(matched)}
						>
							<Download className="size-4" />
						</Button>
						{shareMode && (
							<Button
								size="icon"
								variant="ghost"
								disabled={matched.length === 0}
								title={shareLabel}
								aria-label={shareLabel}
								onClick={() => onShare(matched)}
							>
								{shareMode === "share" ? (
									<Share2 className="size-4" />
								) : (
									<Copy className="size-4" />
								)}
							</Button>
						)}
						<Button
							size="sm"
							variant={follow ? "secondary" : "outline"}
							onClick={() => onFollow(!follow)}
						>
							{follow ? (
								<Pause className="mr-1 size-3.5" />
							) : (
								<Play className="mr-1 size-3.5" />
							)}
							{follow ? m.logs_pause() : m.logs_follow()}
						</Button>
						<Button size="sm" variant="ghost" onClick={onClear}>
							<Trash2 className="mr-1 size-3.5" />
							{m.logs_clear()}
						</Button>
					</div>
				</div>

				{/* A failing poll while lines are already on screen keeps them there — during a host
				    restart the last lines before it went away are the interesting ones — but says so,
				    instead of letting a frozen view read as a quiet host. */}
				{error != null && entries.length > 0 && (
					<p
						role="status"
						className="rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive"
					>
						{m.logs_stalled()}
					</p>
				)}

				<div
					ref={listRef}
					className="max-h-[65vh] overflow-auto rounded-md border bg-card/40 p-2 font-mono text-xs leading-5"
				>
					{visible.length === 0 ? (
						// An empty list has three quite different causes and used to render one sentence
						// for all of them: the host is quiet, the request failed, or it hasn't answered yet.
						<div className="p-2">
							{error ? (
								<div className="space-y-2 font-sans">
									<p className="text-destructive">{m.common_error()}</p>
									{onRetry && (
										<Button size="sm" variant="outline" onClick={onRetry}>
											{m.common_retry()}
										</Button>
									)}
								</div>
							) : (
								<p className="text-muted-foreground">
									{isLoading
										? m.common_loading()
										: // "No plugin output" has a specific, actionable cause that the generic
											// "adjust the filter" line actively misdirects from: the runner is a
											// separate service and is opt-in on Linux, so the usual reason for an
											// empty Plugins view is that it simply isn't running.
											source === "plugins"
											? m.logs_empty_plugins()
											: m.logs_empty()}
								</p>
							)}
						</div>
					) : (
						visible.map((e) => (
							<div key={e.seq} className="whitespace-pre-wrap break-words">
								<span className="text-muted-foreground">
									{fmtTime(e.ts_ms)}{" "}
								</span>
								<span
									className={cn(
										"font-medium",
										LEVEL_CLASS[e.level] ?? "text-muted-foreground",
									)}
								>
									{e.level.padEnd(5)}{" "}
								</span>
								<span className="text-muted-foreground">{e.target} </span>
								<span>{e.msg}</span>
							</div>
						))
					)}
				</div>
			</CardContent>
		</Card>
	);
};

const fmtTime = (ts: number): string => {
	const d = new Date(ts);
	const p = (n: number, w = 2) => String(n).padStart(w, "0");
	return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
};
