// Kit-level error taxonomy. `Data.TaggedError` (matching the SDK's idiom in
// sdk/src/client.ts) — these never cross HTTP; a plugin's UI-API contract defines its own
// Schema-based errors with status annotations.
import { Data } from "effect";

/** A management-API call through the pf facade failed. */
export class HostRequestError extends Data.TaggedError("HostRequestError")<{
	readonly method: string;
	readonly path: string;
	readonly cause: unknown;
}> {}

/** config.json exists but does not parse/decode. */
export class ConfigParseError extends Data.TaggedError("ConfigParseError")<{
	readonly path: string;
	readonly issue: string;
}> {}

/**
 * config.json is group/world-writable (POSIX). This file controls commands run as the
 * host user, so the kit refuses it — the same sshd rule the runner applies to unit files.
 */
export class ConfigPermissionError extends Data.TaggedError(
	"ConfigPermissionError",
)<{
	readonly path: string;
	readonly mode: number;
}> {
	override get message(): string {
		return `refusing ${this.path}: it is group/world-writable (chmod go-w it first) — this file controls commands run as the host user`;
	}
}

/** Persisting config/state failed. */
export class ConfigWriteError extends Data.TaggedError("ConfigWriteError")<{
	readonly path: string;
	readonly cause: unknown;
}> {}

/** The plugin UI server could not be started/registered. */
export class UiServeError extends Data.TaggedError("UiServeError")<{
	readonly cause: unknown;
}> {}

/** A sync pass failed (compute or apply). */
export class SyncError extends Data.TaggedError("SyncError")<{
	readonly reason: string;
	readonly cause: unknown;
}> {}
