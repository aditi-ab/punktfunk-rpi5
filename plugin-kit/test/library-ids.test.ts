import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { isHttpUrl } from "../src/metadata/rules.js";
import { validEntryId } from "../src/ui-server.js";

interface Case {
	value: string;
	fill?: string;
	count?: number;
	valid: boolean;
}
const vectors = JSON.parse(
	readFileSync(
		join(import.meta.dir, "../../clients/shared/library-id-vectors.json"),
		"utf8",
	),
) as { entry_ids: Case[]; urls: Case[] };
const expand = (c: Case) => c.value + (c.fill ?? "").repeat(c.count ?? 0);

describe("the host's library id and URL rules", () => {
	for (const c of vectors.entry_ids) {
		const id = expand(c);
		test(`entry id ${JSON.stringify(id.slice(0, 40))} (${id.length})`, () => {
			expect(validEntryId(id)).toBe(c.valid);
		});
	}
	for (const c of vectors.urls) {
		const url = expand(c);
		test(`url ${JSON.stringify(url.slice(0, 40))} (${url.length})`, () => {
			expect(isHttpUrl(url)).toBe(c.valid);
		});
	}
});
