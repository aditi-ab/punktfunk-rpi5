import { Effect } from "effect";
import type { HostRequestError } from "./errors.js";
import { HostClient } from "./host-client.js";
import { dirAccess } from "./library/parsers/fs.js";

export interface AccessRequestPath {
	readonly path: string;
	readonly write?: boolean;
}

export interface AccessRequestOutcome {
	readonly path: string;
	readonly outcome: string;
}

let oldHostWarned = false;

const statusOf = (value: unknown): number | undefined => {
	if (typeof value !== "object" || value === null) return undefined;
	const record = value as Record<string, unknown>;
	if (typeof record.status === "number") return record.status;
	if (typeof record.statusCode === "number") return record.statusCode;
	return statusOf(record.cause);
};

/** Ask the host to put these folders before the operator; this never grants access itself. */
export const requestAccess = (
	paths: ReadonlyArray<string | AccessRequestPath>,
	reason?: string,
): Effect.Effect<AccessRequestOutcome[], HostRequestError, HostClient> =>
	Effect.gen(function* () {
		const host = yield* HostClient;
		const body = {
			paths: paths.map((entry) =>
				typeof entry === "string" ? { path: entry } : entry,
			),
			...(reason ? { reason } : {}),
		};
		return yield* host.request("POST", "/plugin-access/requests", body).pipe(
			Effect.map((value) =>
				(Array.isArray(value) ? value : []).filter(
					(row): row is AccessRequestOutcome =>
						typeof row === "object" &&
						row !== null &&
						typeof (row as { path?: unknown }).path === "string" &&
						typeof (row as { outcome?: unknown }).outcome === "string",
				),
			),
			Effect.catch((error) => {
				if (statusOf(error.cause) !== 404) return Effect.fail(error);
				if (oldHostWarned) return Effect.succeed([]);
				oldHostWarned = true;
				return Effect.logWarning(
					"the host is too old for folder access requests — update it to let plugins ask for launcher folders",
				).pipe(Effect.as([]));
			}),
		);
	});

/** Folders unusable now. Inside a sandbox, missing may mean merely unbound, so ask the host. */
export const unreachable = (dirs: ReadonlyArray<string>): string[] => {
	const sandboxed = !!process.env.PUNKTFUNK_MGMT_UNIX;
	return [...new Set(dirs)].filter((dir) => {
		const access = dirAccess(dir);
		return access === "denied" || (sandboxed && access === "missing");
	});
};
