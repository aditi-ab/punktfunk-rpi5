// Which registry ports the plugin proxy is willing to dial.
//
// The port is a value a plugin declares when it registers, so it is attacker-chosen the moment
// anyone can write the registry — and it is pasted straight into a loopback `fetch` by both the
// `/plugin-ui/**` proxy and the health probe. Naming one of OUR listeners there makes the proxy
// dial itself, which on the plugin origin recurses until the process dies; and it is silent, since
// a self-dial answers 200 like anything else.
import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { isDialablePort, PLUGIN_ID_RE, validEntryId } from "./pluginProxy";

const CONSOLE_PORT = "47992";
const PLUGIN_PORT = "47993";

afterEach(() => {
	delete process.env.PUNKTFUNK_UI_CONSOLE_PORT_ACTIVE;
	delete process.env.PUNKTFUNK_UI_PLUGIN_PORT_ACTIVE;
});

describe("isDialablePort", () => {
	test("refuses our own two listeners", () => {
		process.env.PUNKTFUNK_UI_CONSOLE_PORT_ACTIVE = CONSOLE_PORT;
		process.env.PUNKTFUNK_UI_PLUGIN_PORT_ACTIVE = PLUGIN_PORT;
		expect(isDialablePort(47992)).toBe(false);
		expect(isDialablePort(47993)).toBe(false);
	});

	test("allows an ordinary plugin port", () => {
		process.env.PUNKTFUNK_UI_CONSOLE_PORT_ACTIVE = CONSOLE_PORT;
		process.env.PUNKTFUNK_UI_PLUGIN_PORT_ACTIVE = PLUGIN_PORT;
		expect(isDialablePort(51234)).toBe(true);
	});

	test("refuses a malformed port rather than pasting it into a URL", () => {
		for (const port of [0, -1, 1.5, 65536, Number.NaN]) {
			expect(isDialablePort(port)).toBe(false);
		}
	});

	test("an unbound plugin origin does not make every port refusable", () => {
		// `pluginOriginPort()` is null when the second listener failed to bind. A null must not
		// collapse into "matches nothing dialable" — plugin UIs are already disabled in that state,
		// but the health probe still runs.
		process.env.PUNKTFUNK_UI_CONSOLE_PORT_ACTIVE = CONSOLE_PORT;
		expect(isDialablePort(51234)).toBe(true);
		expect(isDialablePort(47992)).toBe(false);
	});
});

describe("PLUGIN_ID_RE", () => {
	// This regex is narrower than the host's provider rule (`[a-z0-9._-]`, leading alphanumeric),
	// so a source the host happily lists can be refused here. That refusal is a bad request, not a
	// missing settings surface: `/api/plugin-config/<id>` answers 400, leaving 404 to the plugin
	// declining a `__config` — the one case the console offers the plugin's own page for.
	test("is narrower than a provider id the host will list", () => {
		for (const id of ["my-provider.v2", "rom_manager", "9lives"]) {
			expect(PLUGIN_ID_RE.test(id)).toBe(false);
		}
	});

	test("accepts the ids first-party plugins register", () => {
		for (const id of ["playnite", "rom-manager", "steam", "heroic"]) {
			expect(PLUGIN_ID_RE.test(id)).toBe(true);
		}
	});

	test("refuses a segment that could climb out of the proxy prefix", () => {
		const hostile = ["", "..", "-lead", "A", "a b"];
		hostile.push(["a", "b"].join("/"));
		hostile.push("a%2Fb");
		for (const id of hostile) {
			expect(PLUGIN_ID_RE.test(id)).toBe(false);
		}
	});
});

describe("validEntryId", () => {
	const vectors = JSON.parse(
		readFileSync(
			join(import.meta.dir, "../../../clients/shared/library-id-vectors.json"),
			"utf8",
		),
	) as {
		entry_ids: {
			value: string;
			fill?: string;
			count?: number;
			valid: boolean;
		}[];
	};
	for (const c of vectors.entry_ids) {
		const id = c.value + (c.fill ?? "").repeat(c.count ?? 0);
		test(`${c.valid ? "accepts" : "refuses"} ${id.slice(0, 40)} (${id.length})`, () => {
			expect(validEntryId(id)).toBe(c.valid);
		});
	}
	test("refuses control characters the host would let through", () => {
		expect(validEntryId("steam:5\u00070")).toBe(false);
	});
});
