import { Check, Clipboard, History } from "lucide-react";
import { type FC, useState } from "react";
import type { SessionSummary } from "@/api/gen/model/sessionSummary";
import { useGetRecentSessions } from "@/api/gen/session/session";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { fmtClockDuration, fmtNumber } from "@/lib/format";
import { m } from "@/paraglide/messages";

/**
 * What the session that just finished came to — the numbers the host has always written to
 * its journal and nobody but an operator with `journalctl` could read.
 *
 * Renders nothing until a session has ended, so a host that has not streamed sees no chrome
 * (the `AttentionCard` rule). Copy hands over the summary verbatim: the card and the API
 * carry one struct, so what is pasted into an issue is what the API would have answered.
 */
export const LastSessionCard: FC = () => {
	// The host holds the last eight in memory; only the newest is on screen. Invalidated by
	// `session.ended` (see `api/events.ts`), so there is nothing to poll.
	const recent = useGetRecentSessions({ query: { retry: false } });
	return <LastSession session={recent.data?.sessions?.[0]} />;
};

/** Why it ended, in the console's own words. Same set the client maps from the QUIC close.
 * Exhaustive on purpose: a reason the host adds must fail this build, not read as a stop. */
function endedLabel(s: SessionSummary): string {
	switch (s.ended) {
		case "local":
			return m.status_last_end_local();
		case "game_exited":
			return m.status_last_end_game_exited();
		case "host_ended":
			return m.status_last_end_host_ended();
		case "host_error":
			return m.status_last_end_host_error();
		case "lost":
			return m.status_last_end_lost();
		case "stopped_by_operator":
			return m.status_last_end_stopped();
		default: {
			const unreached: never = s.ended;
			return unreached;
		}
	}
}

/** The pure half — fed a fixture by the stories, so the empty state is provable. */
export const LastSession: FC<{ session?: SessionSummary }> = ({ session }) => {
	const [copied, setCopied] = useState(false);
	if (!session) return null;

	// The session average where the host kept a span, else the rate it finished on —
	// a single number that moved all session is the one thing worth not printing.
	const span = session.bitrate;
	const facts = [
		fmtClockDuration(session.duration_s, { hours: true }),
		session.mode,
		`${session.codec.toUpperCase()} ${session.bit_depth}-bit ${session.chroma}`,
		span
			? m.status_last_bitrate_avg({ mbit: fmtNumber(span.avg_kbps / 1000, 1) })
			: m.status_last_bitrate({
					mbit: fmtNumber(session.bitrate_kbps / 1000, 1),
				}),
		span &&
			span.adaptive_steps > 0 &&
			m.status_last_steps({ count: span.adaptive_steps }),
		session.frames_dropped != null &&
			m.status_last_dropped({ count: session.frames_dropped }),
		session.audio && m.status_last_audio_late({ count: session.audio.late }),
		session.gyro && m.status_last_gyro({ count: session.gyro.stalls }),
		session.hdr && "HDR",
		session.join && m.status_last_join(),
		m.status_last_ended({ reason: endedLabel(session) }),
	].filter(Boolean) as string[];

	const copy = () => {
		try {
			// The API answer itself, not a re-description of it: two spellings of these
			// numbers is one more than a bug report can be trusted to agree with.
			navigator.clipboard.writeText(JSON.stringify(session, null, 2));
			setCopied(true);
			setTimeout(() => setCopied(false), 1500);
		} catch {
			// Clipboard denied (insecure origin, or the user said no). The numbers are on
			// screen, so there is nothing worth interrupting them about.
		}
	};

	return (
		<Card>
			<CardHeader className="flex flex-col items-start gap-3 space-y-0 sm:flex-row sm:items-center sm:justify-between">
				<CardTitle className="flex items-center gap-2">
					<History className="size-4" />
					{m.status_last_title()}
					<span className="text-xs font-normal text-muted-foreground tabular-nums">
						#{session.id}
					</span>
				</CardTitle>
				<Button variant="outline" size="sm" onClick={copy}>
					{copied ? (
						<Check className="size-3.5 text-[var(--success)]" />
					) : (
						<Clipboard className="size-3.5" />
					)}
					{copied ? m.status_last_copied() : m.status_last_copy()}
				</Button>
			</CardHeader>
			<CardContent>
				<p className="flex flex-wrap items-baseline gap-x-2 gap-y-1 text-sm text-muted-foreground tabular-nums">
					{facts.map((fact, i) => (
						// Facts are a fixed list in a fixed order; the index is their identity.
						<span key={`${i}-${fact}`}>
							{i > 0 && <span className="mr-2">·</span>}
							{fact}
						</span>
					))}
				</p>
			</CardContent>
		</Card>
	);
};
