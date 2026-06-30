import type { GameEntry } from "@/api/gen/model/gameEntry";

/** The custom-CRUD path param is the raw id without the `custom:` prefix. */
export function customId(entry: GameEntry): string {
	return entry.id.startsWith("custom:")
		? entry.id.slice("custom:".length)
		: entry.id;
}
