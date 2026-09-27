import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { isHttpUrl } from "./metadata";

const vectors = JSON.parse(
	readFileSync(
		join(import.meta.dir, "../../../clients/shared/library-id-vectors.json"),
		"utf8",
	),
) as {
	urls: { value: string; fill?: string; count?: number; valid: boolean }[];
};

for (const c of vectors.urls) {
	const url = c.value + (c.fill ?? "").repeat(c.count ?? 0);
	test(`${c.valid ? "accepts" : "refuses"} ${JSON.stringify(url.slice(0, 40))} (${url.length})`, () => {
		expect(isHttpUrl(url)).toBe(c.valid);
	});
}
