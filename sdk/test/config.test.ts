// Connection/config resolution helpers. `pluginStateDir` is the writable location a supervised
// plugin persists into — the one dir the de-privileged Windows runner may write.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import * as path from "node:path";
import { pluginStateDir } from "../src/config.js";

describe("pluginStateDir", () => {
	let saved: string | undefined;
	beforeEach(() => {
		saved = process.env.PUNKTFUNK_CONFIG_DIR;
	});
	afterEach(() => {
		if (saved === undefined) delete process.env.PUNKTFUNK_CONFIG_DIR;
		else process.env.PUNKTFUNK_CONFIG_DIR = saved;
	});

	test("resolves <config_dir>/plugin-state[/name] and honors the config-dir override", () => {
		process.env.PUNKTFUNK_CONFIG_DIR = path.join("/tmp", "pf-cfg");
		expect(pluginStateDir()).toBe(path.join("/tmp", "pf-cfg", "plugin-state"));
		expect(pluginStateDir("rom-manager")).toBe(
			path.join("/tmp", "pf-cfg", "plugin-state", "rom-manager"),
		);
	});

	test("the per-plugin dir is nested under the shared root", () => {
		process.env.PUNKTFUNK_CONFIG_DIR = path.join("/tmp", "pf-cfg2");
		expect(pluginStateDir("x").startsWith(pluginStateDir())).toBe(true);
	});
});
