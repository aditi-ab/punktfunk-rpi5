// The parity harness is the release gate for every extracted scanner, so the thing that decides
// pass/fail needs its own tests. The cases below are the ones that actually happen during a port:
// a lost title, a wrong id, a dropped launch recipe, art whose representation changed but whose
// presence didn't, and the launcher entries the plugin legitimately adds.
import { describe, expect, test } from "bun:test";
import {
	claimedLibraryId,
	diffParity,
	formatParityReport,
	fromHostEntry,
	fromProviderEntry,
	type HostGameEntry,
} from "../src/library/parity.js";
import type { ProviderEntry } from "../src/wire.js";

/** What the host reports while its BUILT-IN steam scanner is producing the library. */
const hostEntry = (over: Partial<HostGameEntry> = {}): HostGameEntry => ({
	id: "steam:440",
	store: "steam",
	title: "Team Fortress 2",
	launch: { kind: "steam_appid", value: "440" },
	// The scanner emits host-relative proxy paths the CLIENT resolves.
	art: {
		portrait: "/api/v1/library/art/steam:440/portrait",
		hero: "/api/v1/library/art/steam:440/hero",
		logo: null,
		header: "/api/v1/library/art/steam:440/header",
	},
	platform: "PC",
	...over,
});

/** What the extracted plugin produces for the same title. */
const pluginEntry = (over: Partial<ProviderEntry> = {}): ProviderEntry =>
	({
		external_id: "440",
		title: "Team Fortress 2",
		launch: { kind: "steam_appid", value: "440" },
		// The plugin emits file:// paths and CDN URLs — a DIFFERENT representation of the same art.
		art: {
			portrait: "file:///home/u/.steam/appcache/librarycache/440/a/p.jpg",
			hero: "https://cdn.cloudflare.steamstatic.com/steam/apps/440/library_hero.jpg",
			header: "https://cdn.cloudflare.steamstatic.com/steam/apps/440/header.jpg",
		},
		platform: "PC",
		...over,
	}) as ProviderEntry;

describe("id mapping", () => {
	test("a claimed entry's id is the scanner's id", () => {
		// The whole migration rests on this one line: Moonlight pins, GameStream app ids and client
		// art caches are all derived from it.
		expect(claimedLibraryId("steam", "440")).toBe("steam:440");
		expect(claimedLibraryId("heroic", "legendary:Quail")).toBe(
			"heroic:legendary:Quail",
		);
	});
});

describe("diffParity", () => {
	const base = [fromHostEntry(hostEntry())];

	test("a faithful port passes, even though the art VALUES all changed", () => {
		const r = diffParity(base, [fromProviderEntry("steam", pluginEntry())]);
		expect(r.ok).toBe(true);
		expect(r.matched).toBe(1);
		expect(r.changed).toEqual([]);
		expect(formatParityReport(r)).toContain("parity OK");
	});

	test("a lost title is reported as missing", () => {
		const r = diffParity(base, []);
		expect(r.ok).toBe(false);
		expect(r.missing.map((e) => e.id)).toEqual(["steam:440"]);
		expect(formatParityReport(r)).toContain("missing:  steam:440");
	});

	test("a wrong id shows up as BOTH missing and extra — the loudest failure", () => {
		// The exact shape of the bug this harness exists to catch: the plugin found the title, but
		// under an id nothing downstream recognizes.
		const r = diffParity(base, [
			fromProviderEntry("steam", pluginEntry({ external_id: "440.0" })),
		]);
		expect(r.ok).toBe(false);
		expect(r.missing.map((e) => e.id)).toEqual(["steam:440"]);
		expect(r.extra.map((e) => e.id)).toEqual(["steam:440.0"]);
	});

	test("a changed launch recipe is caught", () => {
		const r = diffParity(base, [
			fromProviderEntry(
				"steam",
				pluginEntry({ launch: { kind: "command", value: "steam" } }),
			),
		]);
		expect(r.ok).toBe(false);
		expect(r.changed).toEqual([
			{
				id: "steam:440",
				field: "launch",
				before: "steam_appid:440",
				after: "command:steam",
			},
		]);
	});

	test("a dropped launch recipe is caught (an unlaunchable tile)", () => {
		const r = diffParity(base, [
			fromProviderEntry("steam", pluginEntry({ launch: null })),
		]);
		expect(r.changed.map((c) => c.field)).toEqual(["launch"]);
	});

	test("LOSING an art kind fails; gaining one does not", () => {
		const lost = diffParity(base, [
			fromProviderEntry("steam", pluginEntry({ art: { portrait: null } })),
		]);
		expect(lost.ok).toBe(false);
		expect(lost.changed.map((c) => c.field)).toContain("art.portrait");

		// The baseline had no logo; the plugin resolves one. That is an improvement, and failing the
		// run over it would only train people to ignore the harness.
		const gained = diffParity(base, [
			fromProviderEntry(
				"steam",
				pluginEntry({
					art: { ...pluginEntry().art, logo: "file:///l.png" },
				}),
			),
		]);
		expect(gained.ok).toBe(true);
	});

	test("metadata drift is caught, but absent-vs-empty is not drift", () => {
		const changed = diffParity(base, [
			fromProviderEntry("steam", pluginEntry({ platform: "Linux" })),
		]);
		expect(changed.changed).toEqual([
			{ id: "steam:440", field: "meta.platform", before: "PC", after: "Linux" },
		]);
		// The host omits empty lists and nulls; a plugin sending them has changed nothing.
		const noise = diffParity(base, [
			fromProviderEntry(
				"steam",
				pluginEntry({ genres: [], tags: [], region: null } as never),
			),
		]);
		expect(noise.ok).toBe(true);
	});

	test("launcher entries are expected extras, not failures", () => {
		// The built-in scanner had no concept of a launcher entry, so it can never be in the
		// baseline — reporting it as `extra` would fail every steam run forever.
		const r = diffParity(base, [
			fromProviderEntry("steam", pluginEntry()),
			fromProviderEntry(
				"steam",
				pluginEntry({
					external_id: "ui:bigpicture",
					title: "Steam Big Picture",
					role: "launcher",
					launch: { kind: "steam_ui", value: "bigpicture" },
					art: {},
				} as never),
			),
		]);
		expect(r.ok).toBe(true);
		expect(r.extra).toEqual([]);
		expect(r.launchersAdded.map((e) => e.id)).toEqual(["steam:ui:bigpicture"]);
		expect(formatParityReport(r)).toContain("+1 launcher entry");
	});

	test("an ordinary title the scanner never had IS a failure", () => {
		// The mirror of the case above: only `role: "launcher"` gets the exemption, so a plugin that
		// invents games (a bad filter, a tool listed as a game) still fails.
		const r = diffParity(base, [
			fromProviderEntry("steam", pluginEntry()),
			fromProviderEntry(
				"steam",
				pluginEntry({ external_id: "228980", title: "Steamworks Common" }),
			),
		]);
		expect(r.ok).toBe(false);
		expect(r.extra.map((e) => e.id)).toEqual(["steam:228980"]);
	});
});
