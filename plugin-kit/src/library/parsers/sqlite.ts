// Read-only SQLite over `bun:sqlite` — for launcher databases a plugin must never disturb.
//
// Lutris' `pga.db` is the motivating case: it belongs to a running application, and a scanner that
// opened it read-write could take a write lock, create `-wal`/`-shm` sidecars next to it, or (worst
// case) be blamed for a corrupted library. `immutable=1` promises the file will not change while
// open, which makes Bun skip locking entirely — the strictest possible "look, don't touch".
import { Database } from "bun:sqlite";
import { isFile } from "./fs.js";

export interface ReadOnlyDb {
	/** Run a query and return its rows. Returns `[]` rather than throwing on a bad query. */
	readonly query: <T = Record<string, unknown>>(
		sql: string,
		...params: unknown[]
	) => T[];
	readonly close: () => void;
}

/**
 * Open a launcher database read-only and immutably. `undefined` if the file is absent or not a
 * database — the normal "this launcher isn't installed" case, not an error.
 *
 * Always `close()` when done (or use {@link withReadOnlyDb}, which does it for you).
 */
export const openReadOnly = (file: string): ReadOnlyDb | undefined => {
	if (!isFile(file)) return undefined;
	let db: Database;
	try {
		// `readonly` alone still takes locks and can spawn WAL sidecars; `immutable=1` is what makes
		// this a pure read. It is safe here precisely because a scan is a point-in-time snapshot —
		// if the launcher writes mid-scan we simply pick it up on the next sync.
		db = new Database(`file:${encodeURI(file)}?immutable=1`, { readonly: true });
	} catch {
		return undefined;
	}
	return {
		query: <T = Record<string, unknown>>(sql: string, ...params: unknown[]) => {
			try {
				return db.query(sql).all(...(params as never[])) as T[];
			} catch {
				// A schema drift (a renamed column in a launcher upgrade) must degrade to "no
				// titles from this source", never take the whole plugin down.
				return [] as T[];
			}
		},
		close: () => {
			try {
				db.close();
			} catch {
				/* already closed */
			}
		},
	};
};

/** Open, use, and always close. Returns `undefined` when the database isn't there. */
export const withReadOnlyDb = <T>(
	file: string,
	use: (db: ReadOnlyDb) => T,
): T | undefined => {
	const db = openReadOnly(file);
	if (!db) return undefined;
	try {
		return use(db);
	} finally {
		db.close();
	}
};
