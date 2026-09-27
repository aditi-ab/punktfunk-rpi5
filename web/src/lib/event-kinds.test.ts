import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { EVENT_KINDS, HOST_EVENT_KINDS } from "./event-kinds";

const spec = JSON.parse(
	readFileSync(join(import.meta.dir, "../../../api/openapi.json"), "utf8"),
) as {
	components: {
		schemas: {
			EventKind: { oneOf: { properties: { kind: { enum: string[] } } }[] };
		};
	};
};

test("the console knows every kind the host publishes", () => {
	const host = spec.components.schemas.EventKind.oneOf.map(
		(o) => o.properties.kind.enum[0],
	);
	expect([...HOST_EVENT_KINDS].sort()).toEqual(host.sort());
});

test("a wildcard leads its domain in the hook picker", () => {
	expect(EVENT_KINDS.slice(0, 3)).toEqual([
		"client.*",
		"client.connected",
		"client.disconnected",
	]);
	expect(EVENT_KINDS.filter((k) => k.endsWith(".*"))).toHaveLength(6);
	expect(EVENT_KINDS).toContain("game.launching");
	expect(EVENT_KINDS).toContain("host.stopping");
});
