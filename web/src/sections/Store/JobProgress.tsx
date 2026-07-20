import { useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, RotateCw, X, XCircle } from "lucide-react";
import { type FC, useEffect, useRef } from "react";
import { invalidateStore, type StoreJob, useStoreJob } from "@/api/store";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { m } from "@/paraglide/messages";

// The host reports a job's progress as a free-form phase string; these are the ones it emits, in
// order. Anything unrecognised falls through to the raw phase rather than an empty label, so a new
// host phase degrades to English-but-visible instead of disappearing.
const PHASES: Record<string, () => string> = {
	queued: () => m.store_phase_queued(),
	verifying: () => m.store_phase_verifying(),
	installing: () => m.store_phase_installing(),
	removing: () => m.store_phase_removing(),
	checking: () => m.store_phase_checking(),
	"rolling back": () => m.store_phase_rolling_back(),
	recording: () => m.store_phase_recording(),
	"restarting runner": () => m.store_phase_restarting(),
	done: () => m.store_phase_done(),
};

const phaseLabel = (phase: string): string => PHASES[phase]?.() ?? phase;

/** Keep the tail — an install log can run long and only the end is ever interesting. */
const LOG_TAIL = 200;

/**
 * Container: the in-flight install/uninstall. Polls the job once a second while it runs (the query
 * stops polling itself once the job settles), and refreshes everything the job touched — catalog,
 * installed list, runner state, plugin directory — the moment it finishes.
 */
export const JobProgressSection: FC<{
	jobId: string;
	onDismiss: () => void;
}> = ({ jobId, onDismiss }) => {
	const qc = useQueryClient();
	const job = useStoreJob(jobId);
	const settled = job.data?.state === "done" || job.data?.state === "failed";
	// Refresh once per job, not on every re-render while the finished card sits there.
	const refreshed = useRef<string | null>(null);

	useEffect(() => {
		if (!settled || refreshed.current === jobId) return;
		refreshed.current = jobId;
		invalidateStore(qc);
	}, [settled, jobId, qc]);

	if (!job.data) return null;
	return <JobProgressCard job={job.data} onDismiss={onDismiss} />;
};

/** The progress card: phase (or outcome), a collapsible log tail, and the failure reason if any. */
export const JobProgressCard: FC<{
	job: StoreJob;
	onDismiss: () => void;
}> = ({ job, onDismiss }) => {
	const running = job.state === "running";
	const failed = job.state === "failed";
	const log = job.log.slice(-LOG_TAIL);

	return (
		<Card
			className={failed ? "ring-2 ring-destructive/60" : undefined}
			aria-live="polite"
		>
			<CardContent className="space-y-3 p-card">
				<div className="flex items-start gap-3">
					{running ? (
						<Spinner className="mt-0.5 size-5 shrink-0" />
					) : failed ? (
						<XCircle className="mt-0.5 size-5 shrink-0 text-destructive" />
					) : (
						<CheckCircle2 className="mt-0.5 size-5 shrink-0 text-[var(--success)]" />
					)}
					<div className="min-w-0 flex-1">
						<p className="text-sm font-medium">
							{job.kind === "uninstall"
								? m.store_job_uninstall({ target: job.target })
								: m.store_job_install({ target: job.target })}
						</p>
						<p className="text-sm text-muted-foreground">
							{running
								? phaseLabel(job.phase)
								: failed
									? m.store_job_failed()
									: job.kind === "uninstall"
										? m.store_job_done_uninstall()
										: m.store_job_done_install()}
						</p>
					</div>
					{!running && (
						<Button
							variant="ghost"
							size="icon"
							aria-label={m.store_job_dismiss()}
							onClick={onDismiss}
						>
							<X className="size-4" />
						</Button>
					)}
				</div>

				{failed && job.error && (
					<p className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
						{job.error}
					</p>
				)}

				{/* The runner is bounced at the end of every successful job, so the plugin's nav entry
				    (and its UI) takes a moment longer to appear than the job's "done". Say so. */}
				{job.state === "done" && (
					<p className="flex items-center gap-2 text-xs text-muted-foreground">
						<RotateCw className="size-3.5" />
						{m.store_job_restarting()}
					</p>
				)}

				{log.length > 0 && (
					<details className="group">
						<summary className="cursor-pointer list-none text-xs text-muted-foreground transition-colors hover:text-foreground">
							{m.store_job_log()}
						</summary>
						<pre className="mt-2 max-h-64 overflow-auto rounded-md bg-muted p-3 text-left text-xs text-muted-foreground">
							<code>{log.join("\n")}</code>
						</pre>
					</details>
				)}
			</CardContent>
		</Card>
	);
};
