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

	// Probed after mount: the server render has no `navigator`, and guessing there would mismatch
	// on hydration. Until then the share button is simply absent.
	useEffect(() => {
		setShareMode(detectShareMode());
	}, []);

	const query = useLogsGet(
		{ after: cursor > 0 ? cursor : undefined },
		{ query: { refetchInterval: follow ? 2_000 : false } },
	);

	const data = query.data;
	useEffect(() => {
		if (!data || data.entries.length === 0) return;
		setEntries((prev) => {
			// Only append entries newer than what we already hold — dedup by the monotonic `seq`.
			// Guards a double-invoked mount effect (React StrictMode, or `data` warm in cache) from
			// appending the same page twice (duplicate rows + duplicate React keys).
			const lastSeq = prev.at(-1)?.seq ?? -1;
			const fresh = data.entries.filter((e) => e.seq > lastSeq);
			return fresh.length ? [...prev, ...fresh].slice(-KEEP) : prev;
		});
		setDropped((d) => d || data.dropped);
		setCursor(data.next);
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
}> = ({
	entries,
	follow,
	onFollow,
	onClear,
	onDownload,
	onShare,
	shareMode,
	dropped,
}) => {
	const [minLevel, setMinLevel] = useState<MinLevel>("DEBUG");
	const [search, setSearch] = useState("");
	const listRef = useRef<HTMLDivElement>(null);

	const matched = useMemo(() => {
		const min = RANK[minLevel] ?? 0;
		const q = search.trim().toLowerCase();
		return entries.filter(
			(e) =>
				(RANK[e.level] ?? 0) >= min &&
				(q === "" ||
					e.msg.toLowerCase().includes(q) ||
					e.target.toLowerCase().includes(q)),
		);
	}, [entries, minLevel, search]);
	const visible = useMemo(() => matched.slice(-SHOW), [matched]);
	const shareLabel = shareMode === "share" ? m.logs_share() : m.logs_copy();

	// Keep the tail in view while following (entries are append-only, so length is a good signal).
	useEffect(() => {
		if (!follow) return;
		const el = listRef.current;
		if (el) el.scrollTop = el.scrollHeight;
	}, [follow, visible.length]);

	return (
		<Card>
			<CardContent className="flex flex-col gap-3 pt-6">
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

				<div
					ref={listRef}
					className="max-h-[65vh] overflow-auto rounded-md border bg-card/40 p-2 font-mono text-xs leading-5"
				>
					{visible.length === 0 ? (
						<p className="p-2 text-muted-foreground">{m.logs_empty()}</p>
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
