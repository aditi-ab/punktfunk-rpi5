// Which registry ports the plugin proxy is willing to dial.
//
// The port is a value a plugin declares when it registers, so it is attacker-chosen the moment
// anyone can write the registry — and it is pasted straight into a loopback `fetch` by both the
// `/plugin-ui/**` proxy and the health probe. Naming one of OUR listeners there makes the proxy
// dial itself, which on the plugin origin recurses until the process dies; and it is silent, since
// a self-dial answers 200 like anything else.
import { afterEach, describe, expect, test } from "bun:test";
import { isDialablePort } from "./pluginProxy";

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
