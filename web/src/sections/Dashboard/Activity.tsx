import { Activity as ActivityIcon } from "lucide-react";
import type { FC } from "react";
import { type ActivityEntry, useActivity } from "@/api/events";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { fmtDateTime } from "@/lib/format";
import { m } from "@/paraglide/messages";

/**
 * What this host has been doing — the event stream, rendered.
 *
 * The console could describe the present (a status snapshot) but never the recent past: a client
 * that connected and left while you were on another page left no trace anywhere you could look.
 * The stream was already open for cache invalidation, so this costs one ring buffer.
 *
 * In-memory and bounded, so it starts empty on a page load and fills as things happen. That is the
 * honest shape for a live tail — pretending to be a durable log would need the host to keep one.
 */
export const ActivityCard: FC = () => {
	const entries = useActivity();
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<ActivityIcon className="size-4" />
					{m.activity_title()}
				</CardTitle>
			</CardHeader>
			<CardContent>
				{entries.length === 0 ? (
					<p className="text-sm text-muted-foreground">{m.activity_empty()}</p>
				) : (
					<ul className="flex flex-col divide-y">
						{entries.map((e) => (
							<li
								key={e.seq}
								className="flex flex-wrap items-center gap-x-3 gap-y-1 py-2 first:pt-0 last:pb-0"
							>
								<Badge variant={toneFor(e.kind)}>{kindLabel(e.kind)}</Badge>
								<span className="min-w-0 flex-1 truncate text-sm">
									{describe(e)}
								</span>
								<time
									dateTime={new Date(e.ts_ms).toISOString()}
									className="shrink-0 text-xs tabular-nums text-muted-foreground"
								>
									{fmtDateTime(e.ts_ms)}
								</time>
							</li>
						))}
					</ul>
				)}
			</CardContent>
		</Card>
	);
};

/** The subject of an event, in one line — whatever the payload actually names. */
function describe(e: ActivityEntry): string {
	const d = e.data;
	const client = pick(d.client, "name") ?? pick(d.session, "client");
	const stream = d.stream as Record<string, unknown> | undefined;
	const parts = [
		client,
		typeof stream?.app === "string" ? stream.app : undefined,
		typeof d.reason === "string" ? d.reason : undefined,
		typeof d.game === "string" ? d.game : undefined,
	].filter((x): x is string => typeof x === "string" && x.length > 0);
	// An event whose payload names nothing (host.started, library.changed) is still worth a row —
	// the kind badge carries the whole meaning, so leave the line blank rather than inventing text.
	return parts.join(" · ");
}

/** Read a string field off a nested ref object, tolerating anything unexpected. */
function pick(obj: unknown, key: string): string | undefined {
	if (!obj || typeof obj !== "object") return undefined;
	const v = (obj as Record<string, unknown>)[key];
	if (typeof v === "string") return v;
	// `SessionRef.client` is itself a ClientRef.
	if (v && typeof v === "object") {
		const name = (v as Record<string, unknown>).name;
		return typeof name === "string" ? name : undefined;
	}
	return undefined;
}

/** Colour by what the event means, not by its domain — good news green, losses muted, denials red. */
function toneFor(
	kind: string,
): "success" | "destructive" | "secondary" | "outline" {
	if (kind === "pairing.denied") return "destructive";
	if (kind.endsWith(".connected") || kind.endsWith(".started"))
		return "success";
	if (kind === "pairing.completed") return "success";
	if (kind.endsWith(".disconnected") || kind.endsWith(".ended"))
		return "outline";
	if (kind.endsWith(".stopped") || kind.endsWith(".exited")) return "outline";
	return "secondary";
}

/** Translated label per kind, falling back to the raw kind so a new host event still shows. */
const KIND_LABEL: Record<string, () => string> = {
	"client.connected": () => m.activity_client_connected(),
	"client.disconnected": () => m.activity_client_disconnected(),
	"session.started": () => m.activity_session_started(),
	"session.ended": () => m.activity_session_ended(),
	"stream.started": () => m.activity_stream_started(),
	"stream.stopped": () => m.activity_stream_stopped(),
	"game.running": () => m.activity_game_running(),
	"game.exited": () => m.activity_game_exited(),
	"pairing.pending": () => m.activity_pairing_pending(),
	"pairing.completed": () => m.activity_pairing_completed(),
	"pairing.denied": () => m.activity_pairing_denied(),
	"display.created": () => m.activity_display_created(),
	"display.released": () => m.activity_display_released(),
	"library.changed": () => m.activity_library_changed(),
	"update.available": () => m.activity_update_available(),
	"update.applied": () => m.activity_update_applied(),
	"plugins.changed": () => m.activity_plugins_changed(),
	"store.changed": () => m.activity_store_changed(),
	"host.started": () => m.activity_host_started(),
	"host.stopping": () => m.activity_host_stopping(),
};

function kindLabel(kind: string): string {
	return KIND_LABEL[kind]?.() ?? kind;
}
