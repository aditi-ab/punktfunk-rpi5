import type { UpdateStatus } from "@/api/gen/model";

/** Give up waiting for the host to come back this long after apply started. */
export const APPLY_TIMEOUT_MS = 8 * 60 * 1000;

export type Applying = { target: string; startedAt: number } | null;

/**
 * The wait on a host job this page did not start (a reloaded page): `since` is the first time
 * the job was seen, `gaveUp` means the operator stopped waiting on it.
 */
export type JobWait = { since: number; gaveUp: boolean } | null;

/**
 * The job clock to hold. It starts at the first sighting of a job with no local apply, keeps that
 * time while the job stays, and ends with the job. Returns `held` itself when nothing changes.
 */
export function nextJobWait(
	hasJob: boolean,
	applying: Applying,
	held: JobWait,
	now: number,
): JobWait {
	if (!hasJob) return null;
	return held ?? (applying ? null : { since: now, gaveUp: false });
}

/** The host reports a job the operator has not given up on. */
export const jobRunning = (s: UpdateStatus | undefined, wait: JobWait) =>
	Boolean(s?.job) && !wait?.gaveUp;

/** The "latest version" row. */
export type LatestVersion =
	| { kind: "published"; version: string; notesUrl: string }
	| { kind: "empty-channel" }
	| { kind: "never-checked" };

/** What sits under the two version rows. */
export type UpdatePanel =
	| { kind: "progress" }
	| { kind: "apply" }
	| { kind: "how-to"; optInHint: string | null }
	| { kind: "up-to-date" }
	| { kind: "none" };

/** The check button, or why there is none. */
export type CheckRow =
	| { kind: "disabled" }
	| { kind: "button"; lastChecked: number | null }
	| { kind: "none" };

interface UpdateBody {
	status: UpdateStatus;
	latest: LatestVersion;
	panel: UpdatePanel;
	lastResult: NonNullable<UpdateStatus["last_result"]> | null;
	/** The last verified manifest is old. */
	stale: boolean;
	/** The channel has no release yet; not a failed check. */
	emptyChannel: boolean;
	lastError: string | null;
	check: CheckRow;
}

interface UpdateCardView {
	/** An apply was accepted or the host reports a job that was not given up on. */
	inFlight: boolean;
	/** Waiting for the host to return has outlasted a restart. */
	timedOut: boolean;
	/** The status poll is failing, so the rendered snapshot is the last one received. */
	pollFailing: boolean;
	/** The poll error to show; none while an apply restarts the host. */
	error: unknown;
	showBadge: boolean;
	/** `null` until a status has arrived. */
	body: UpdateBody | null;
}

const latestVersion = (s: UpdateStatus): LatestVersion => {
	if (s.manifest) {
		return {
			kind: "published",
			version: s.manifest.version,
			notesUrl: s.manifest.notes_url,
		};
	}
	return { kind: s.not_published ? "empty-channel" : "never-checked" };
};

const updatePanel = (s: UpdateStatus, inFlight: boolean): UpdatePanel => {
	if (inFlight) return { kind: "progress" };
	if (!s.available) return { kind: s.manifest ? "up-to-date" : "none" };
	if (s.apply === "full" || s.apply === "staged") return { kind: "apply" };
	return { kind: "how-to", optInHint: s.opt_in_hint || null };
};

const checkRow = (s: UpdateStatus, inFlight: boolean): CheckRow => {
	if (s.check_disabled) return { kind: "disabled" };
	if (inFlight) return { kind: "none" };
	return { kind: "button", lastChecked: s.last_checked_unix ?? null };
};

const updateBody = (s: UpdateStatus, inFlight: boolean): UpdateBody => ({
	status: s,
	latest: latestVersion(s),
	panel: updatePanel(s, inFlight),
	lastResult: inFlight ? null : (s.last_result ?? null),
	stale: Boolean(s.manifest?.stale),
	emptyChannel: !inFlight && s.not_published,
	lastError: inFlight ? null : s.last_error || null,
	check: checkRow(s, inFlight),
});

/**
 * Everything the card decides from the last status snapshot, the apply in progress, the wait on
 * a host job, the poll error and the clock. The timeout counts from the apply, else from the
 * job's first sighting. A failed poll during an apply is expected: the snapshot stays on screen.
 */
export function updateCardView(
	s: UpdateStatus | undefined,
	applying: Applying,
	wait: JobWait,
	error: unknown,
	now: number,
): UpdateCardView {
	const hostJob = jobRunning(s, wait);
	const inFlight = Boolean(applying) || hostJob;
	const since = applying?.startedAt ?? (hostJob ? wait?.since : undefined);
	return {
		inFlight,
		timedOut: since !== undefined && now - since > APPLY_TIMEOUT_MS,
		pollFailing: Boolean(error),
		error: inFlight ? undefined : error,
		showBadge: Boolean(s?.available) && !inFlight,
		body: s ? updateBody(s, inFlight) : null,
	};
}
