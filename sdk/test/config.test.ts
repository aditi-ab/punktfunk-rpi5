// Connection/config resolution helpers. `pluginStateDir` is the writable location a supervised
// plugin persists into — the one dir the de-privileged Windows runner may write.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import {
	pluginIngestDir,
	pluginStateDir,
	publishedMgmtUrl,
	resolveConfig,
} from "../src/config.js";

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

describe("pluginIngestDir", () => {
	let saved: string | undefined;
	beforeEach(() => {
		saved = process.env.PUNKTFUNK_CONFIG_DIR;
	});
	afterEach(() => {
		if (saved === undefined) delete process.env.PUNKTFUNK_CONFIG_DIR;
		else process.env.PUNKTFUNK_CONFIG_DIR = saved;
	});

	test("resolves <config_dir>/ingest[/name], distinct from plugin-state", () => {
		process.env.PUNKTFUNK_CONFIG_DIR = path.join("/tmp", "pf-cfg3");
		expect(pluginIngestDir()).toBe(path.join("/tmp", "pf-cfg3", "ingest"));
		expect(pluginIngestDir("playnite")).toBe(
			path.join("/tmp", "pf-cfg3", "ingest", "playnite"),
		);
		// the inbox (Users-write) is a different tree from state (LocalService-write)
		expect(pluginIngestDir("playnite")).not.toBe(pluginStateDir("playnite"));
	});
});

describe("publishedMgmtUrl / resolveConfig url", () => {
	let saved: Record<string, string | undefined>;
	let dir: string;
	beforeEach(() => {
		saved = {
			PUNKTFUNK_CONFIG_DIR: process.env.PUNKTFUNK_CONFIG_DIR,
			PUNKTFUNK_MGMT_URL: process.env.PUNKTFUNK_MGMT_URL,
			PUNKTFUNK_MGMT_TOKEN: process.env.PUNKTFUNK_MGMT_TOKEN,
		};
		dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-endpoint-"));
		process.env.PUNKTFUNK_CONFIG_DIR = dir;
		delete process.env.PUNKTFUNK_MGMT_URL;
		process.env.PUNKTFUNK_MGMT_TOKEN = "t"; // resolveConfig needs SOME token
	});
	afterEach(() => {
		for (const [k, v] of Object.entries(saved)) {
			if (v === undefined) delete process.env[k];
			else process.env[k] = v;
		}
		fs.rmSync(dir, { recursive: true, force: true });
	});

	test("absent file → undefined, and resolveConfig keeps the 47990 default", async () => {
		expect(publishedMgmtUrl()).toBeUndefined();
		expect((await resolveConfig()).url).toBe("https://127.0.0.1:47990");
	});

	test("the host's mgmt-endpoint line is followed — a moved port reaches every plugin", async () => {
		// exactly what `mgmt::endpoint_line` writes (KEY=VALUE, one line)
		fs.writeFileSync(
			path.join(dir, "mgmt-endpoint"),
			"PUNKTFUNK_MGMT_URL=https://127.0.0.1:47995\n",
		);
		expect(publishedMgmtUrl()).toBe("https://127.0.0.1:47995");
		expect((await resolveConfig()).url).toBe("https://127.0.0.1:47995");
	});

	test("an explicit PUNKTFUNK_MGMT_URL still wins over the published file", async () => {
		fs.writeFileSync(
			path.join(dir, "mgmt-endpoint"),
			"PUNKTFUNK_MGMT_URL=https://127.0.0.1:47995\n",
		);
		process.env.PUNKTFUNK_MGMT_URL = "https://127.0.0.1:50000/";
		expect((await resolveConfig()).url).toBe("https://127.0.0.1:50000");
	});

	test("a blank file reads as unset, not as an empty URL", () => {
		fs.writeFileSync(path.join(dir, "mgmt-endpoint"), "\n");
		expect(publishedMgmtUrl()).toBeUndefined();
	});
});
