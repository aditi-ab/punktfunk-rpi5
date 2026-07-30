import type { LogEntry } from "@/api/gen/model/logEntry";

/**
 * How the log entries can leave the page. Probed at runtime rather than assumed: `share` is Web
 * Share level 2 (mobile Safari/Chrome), `copy` is every secure-context desktop browser, and `null`
 * is a page served over plain HTTP to a browser without Web Share — there the clipboard API is
 * absent too, so the button is left out instead of offered as a no-op.
 */
export type ShareMode = "share" | "copy";

export type ShareOutcome = "shared" | "copied" | "cancelled" | "failed";

const MIME = "text/plain";

/**
 * One line per entry, in the on-screen column order but with the full date and UTC offset — a bare
 * wall-clock time is ambiguous the moment the file leaves the browser, and bug reports span days.
 */
export const logsToText = (entries: LogEntry[]): string =>
	entries
		.map((e) => `${stamp(e.ts_ms)} ${e.level.padEnd(5)} ${e.target} ${e.msg}`)
		.join("\n");

/** `punktfunk-logs-20260730-142231.log` — sorts chronologically in a downloads folder. */
export const logFilename = (now: Date): string =>
	`punktfunk-logs-${p(now.getFullYear(), 4)}${p(now.getMonth() + 1)}${p(now.getDate())}-${p(now.getHours())}${p(now.getMinutes())}${p(now.getSeconds())}.log`;

export const downloadText = (text: string, filename: string): void => {
	const url = URL.createObjectURL(new Blob([text], { type: MIME }));
	const a = document.createElement("a");
	a.href = url;
	a.download = filename;
	document.body.appendChild(a);
	a.click();
	a.remove();
	URL.revokeObjectURL(url);
};

export const detectShareMode = (): ShareMode | null => {
	if (typeof navigator === "undefined") return null; // server render
	// `canShare` decides on the file's type, not its bytes, so a stand-in probe file is enough.
	if (canShareFiles([logFile("probe", "punktfunk-logs.log")])) return "share";
	return typeof navigator.clipboard?.writeText === "function" ? "copy" : null;
};

/**
 * Hand the logs to the OS share sheet, falling back to the clipboard. Everything up to the
 * `navigator.share`/`writeText` call is synchronous on purpose: both APIs require the calling task
 * to still be the user's click, and an `await` in between would forfeit that on Safari.
 */
export const shareLogs = async (
	text: string,
	filename: string,
): Promise<ShareOutcome> => {
	const files = [logFile(text, filename)];
	if (canShareFiles(files)) {
		try {
			await navigator.share({ files, title: filename });
			return "shared";
		} catch (err) {
			// A dismissed share sheet is a deliberate cancel, not something to report back.
			return err instanceof DOMException && err.name === "AbortError"
				? "cancelled"
				: "failed";
		}
	}
	try {
		await navigator.clipboard.writeText(text);
		return "copied";
	} catch {
		return "failed";
	}
};

const logFile = (text: string, filename: string): File =>
	new File([text], filename, { type: MIME });

// Callers reach this from a click or from the guarded probe above, so `navigator` is always there.
const canShareFiles = (files: File[]): boolean =>
	typeof navigator.canShare === "function" && navigator.canShare({ files });

const p = (n: number, w = 2): string => String(n).padStart(w, "0");

/** Local ISO 8601 with a numeric offset, e.g. `2026-07-30T14:22:31.123+02:00`. */
const stamp = (ts: number): string => {
	const d = new Date(ts);
	const date = `${p(d.getFullYear(), 4)}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
	const time = `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
	// getTimezoneOffset() counts minutes *behind* UTC, so east of Greenwich is negative.
	const off = -d.getTimezoneOffset();
	const sign = off < 0 ? "-" : "+";
	const abs = Math.abs(off);
	return `${date}T${time}${sign}${p(Math.floor(abs / 60))}:${p(abs % 60)}`;
};
