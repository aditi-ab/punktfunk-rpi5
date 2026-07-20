// Filesystem seam: the SDK owns the canonical path layout (plugin-state, ingest); the kit
// re-exports those and adds the two write primitives every plugin needs — a 0700 state dir
// and atomic (temp + rename, 0600) file writes. All IO is node:fs, wrapped in Effect.
import * as fs from "node:fs";
import * as path from "node:path";
import { pluginIngestDir, pluginStateDir } from "@punktfunk/host";
import { Effect } from "effect";
import { ConfigWriteError } from "./errors.js";

export { pluginIngestDir, pluginStateDir };

/** Create `pluginStateDir(name)` 0700 if absent (idempotent). Returns the dir. */
export const ensureStateDir = (
	name: string,
): Effect.Effect<string, ConfigWriteError> =>
	Effect.try({
		try: () => {
			const dir = pluginStateDir(name);
			fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
			return dir;
		},
		catch: (cause) =>
			new ConfigWriteError({ path: pluginStateDir(name), cause }),
	});

/**
 * Atomic write: temp file (0600) in the same dir, then rename — a crash never leaves a
 * torn file, and readers only ever see complete content.
 */
export const atomicWriteFile = (
	file: string,
	data: string,
): Effect.Effect<void, ConfigWriteError> =>
	Effect.try({
		try: () => {
			const tmp = `${file}.tmp-${process.pid}-${Date.now()}`;
			fs.writeFileSync(tmp, data, { mode: 0o600 });
			fs.renameSync(tmp, file);
		},
		catch: (cause) => new ConfigWriteError({ path: file, cause }),
	});

/** Join inside a plugin's state dir. */
export const statePath = (name: string, file: string): string =>
	path.join(pluginStateDir(name), file);
