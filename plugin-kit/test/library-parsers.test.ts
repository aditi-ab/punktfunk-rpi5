// The parser ports, tested against the SAME cases the host's Rust scanners pin.
//
// These are not "does TypeScript work" tests. The formats here are undocumented and the host's
// versions are the reference implementation; a port that drifts produces a library that looks fine
// and launches nothing. Where a Rust test exists, its assertions are carried over verbatim — the
// per-plugin parity harness (design M5) then checks the whole pipeline against a live host, but
// these catch a drift long before that.
import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import {
	confinedJoin,
	crc32,
	findGridArtFile,
	findLocalArtFile,
	fileUrl,
	gridFilenames,
	isSteamTool,
	parseAppManifest,
	parseRegQuery,
	parseShortcuts,
	readTextCapped,
	shortcutAppId,
	shortcutGameId,
	steamCdnUrl,
	vdfPaths,
	vdfValue,
} from "../src/library/parsers/index.js";

const tmp = (name: string): string => {
	const dir = path.join(os.tmpdir(), `pf-kit-${name}-${process.pid}`);
	fs.mkdirSync(dir, { recursive: true });
	return dir;
};

describe("text VDF / ACF", () => {
	test("vdfValue extracts a quoted field", () => {
		expect(vdfValue('"path"\t\t"/mnt/games/SteamLibrary"', "path")).toBe(
			"/mnt/games/SteamLibrary",
		);
		expect(vdfValue('"appid"\t\t"570"', "appid")).toBe("570");
		expect(vdfValue('"name"\t\t"Dota 2"', "name")).toBe("Dota 2");
		// Wrong key → nothing (a prefix match must not leak the neighbouring field).
		expect(vdfValue('"installdir"\t\t"x"', "appid")).toBeUndefined();
	});

	test("vdfPaths pulls every library folder and unescapes Windows separators", () => {
		const vdf = `
"libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
		"label"		""
	}
	"1"
	{
		"path"		"D:\\\\SteamLibrary"
	}
}`;
		expect(vdfPaths(vdf)).toEqual([
			"/home/u/.local/share/Steam",
			"D:\\SteamLibrary",
		]);
	});

	test("parseAppManifest reads the flat fields it needs", () => {
		const acf = `"AppState"
{
	"appid"		"570"
	"name"		"Dota 2"
	"installdir"		"dota 2 beta"
}`;
		expect(parseAppManifest(acf)).toEqual({
			appid: 570,
			name: "Dota 2",
			installdir: "dota 2 beta",
		});
		// A manifest missing the essentials is not a title.
		expect(parseAppManifest('"AppState" { "name" "x" }')).toBeUndefined();
	});

	test("isSteamTool keeps runtimes out of a game library", () => {
		expect(isSteamTool(228980, "Steamworks Common Redistributables")).toBe(true);
		expect(isSteamTool(1628350, "Steam Linux Runtime 3.0 (sniper)")).toBe(true);
		expect(isSteamTool(999, "Proton 9.0")).toBe(true);
		expect(isSteamTool(999, "SteamVR")).toBe(true);
		expect(isSteamTool(570, "Dota 2")).toBe(false);
	});
});

describe("binary shortcuts.vdf", () => {
	/** Build a binary shortcuts.vdf the way Steam writes one. */
	const buildShortcuts = (
		entries: ReadonlyArray<{
			appid?: number;
			appname: string;
			exe: string;
			hidden?: boolean;
		}>,
	): Uint8Array => {
		const parts: number[] = [];
		const cstr = (s: string) => {
			for (const b of new TextEncoder().encode(s)) parts.push(b);
			parts.push(0);
		};
		const i32 = (v: number) => {
			parts.push(v & 0xff, (v >>> 8) & 0xff, (v >>> 16) & 0xff, (v >>> 24) & 0xff);
		};
		parts.push(0x00);
		cstr("shortcuts");
		entries.forEach((e, i) => {
			parts.push(0x00);
			cstr(String(i));
			if (e.appid !== undefined) {
				parts.push(0x02);
				cstr("appid");
				i32(e.appid);
			}
			parts.push(0x01);
			cstr("AppName");
			cstr(e.appname);
			parts.push(0x01);
			cstr("Exe");
			cstr(e.exe);
			parts.push(0x02);
			cstr("IsHidden");
			i32(e.hidden ? 1 : 0);
			// A nested map the parser must skip wholesale.
			parts.push(0x00);
			cstr("tags");
			parts.push(0x01);
			cstr("0");
			cstr("favourite");
			parts.push(0x08);
			parts.push(0x08); // end of this shortcut
		});
		parts.push(0x08); // end of shortcuts
		parts.push(0x08); // end of document
		return new Uint8Array(parts);
	};

	test("parses entries, skips nested maps, and reads the hidden flag", () => {
		const buf = buildShortcuts([
			{ appid: 2456789012, appname: "My Emulator", exe: '"/usr/bin/foo"' },
			{ appid: 3000000000, appname: "Hidden One", exe: '"/x"', hidden: true },
		]);
		const got = parseShortcuts(buf);
		expect(got).toHaveLength(2);
		expect(got[0]).toMatchObject({
			appid: 2456789012,
			name: "My Emulator",
			hidden: false,
		});
		expect(got[1]).toMatchObject({ name: "Hidden One", hidden: true });
	});

	test("derives the appid when the file omits it", () => {
		const buf = buildShortcuts([{ appname: "No Appid", exe: '"/usr/bin/x"' }]);
		const got = parseShortcuts(buf);
		expect(got).toHaveLength(1);
		// Derived ids always carry the high bit — that is how a shortcut is told apart from a real
		// store appid downstream (and why its CDN art fetch is skipped).
		expect(got[0].appid & 0x8000_0000).not.toBe(0);
		expect(got[0].appid).toBe(shortcutAppId('"/usr/bin/x"', "No Appid"));
	});

	test("is total on a truncated or garbled file", () => {
		expect(parseShortcuts(new Uint8Array([]))).toEqual([]);
		expect(parseShortcuts(new Uint8Array([0x01, 0x02, 0x03]))).toEqual([]);
		const good = buildShortcuts([{ appid: 1, appname: "A", exe: "/a" }]);
		// Every truncation of a valid file must return, not throw.
		for (let i = 0; i < good.length; i++) {
			expect(() => parseShortcuts(good.subarray(0, i))).not.toThrow();
		}
	});

	test("crc32 matches the IEEE check value", () => {
		// The canonical CRC-32 check: crc32("123456789") == 0xCBF43926.
		expect(crc32(new TextEncoder().encode("123456789"))).toBe(0xcbf4_3926);
	});

	test("shortcutGameId composes the appid and the shortcut marker", () => {
		// high dword = appid, low dword = 0x02000000. Handing rungameid the bare 32-bit appid does
		// NOT launch a shortcut, which is the entire reason this function exists.
		const id = BigInt(shortcutGameId(0x8000_0000));
		expect(id >> 32n).toBe(0x8000_0000n);
		expect(id & 0xffff_ffffn).toBe(0x0200_0000n);
		// Digits only — it rides the `steam_appid` launch kind, which the host validates as digits.
		expect(shortcutGameId(2_456_789_012)).toMatch(/^\d+$/);
	});
});

describe("path confinement", () => {
	test("confinedJoin refuses anything that could escape the install dir", () => {
		const base = path.join(path.sep, "games", "W3");
		expect(confinedJoin(base, "bin/game.exe")).toBe(
			path.join(base, "bin", "game.exe"),
		);
		expect(confinedJoin(base, "bin\\game.exe")).toBe(
			path.join(base, "bin", "game.exe"),
		);
		// The three shapes a crafted goggame-*.info would use to point elsewhere.
		expect(confinedJoin(base, "../../windows/system32/cmd.exe")).toBeUndefined();
		expect(confinedJoin(base, "/etc/passwd")).toBeUndefined();
		expect(confinedJoin(base, "C:\\Windows\\system32\\cmd.exe")).toBeUndefined();
		expect(confinedJoin(base, "")).toBeUndefined();
	});
});

describe("capped reads", () => {
	test("readTextCapped refuses an over-cap file and a missing one", () => {
		const dir = tmp("caps");
		const small = path.join(dir, "small.txt");
		fs.writeFileSync(small, "hello");
		expect(readTextCapped(small)).toBe("hello");
		expect(readTextCapped(small, 2)).toBeUndefined(); // over the cap
		expect(readTextCapped(path.join(dir, "nope.txt"))).toBeUndefined();
		expect(readTextCapped(dir)).toBeUndefined(); // a directory is not a file
		fs.rmSync(dir, { recursive: true, force: true });
	});
});

describe("art locations", () => {
	test("steamCdnUrl skips shortcut appids, which have no CDN entry", () => {
		expect(steamCdnUrl(570, "header")).toContain("/570/header.jpg");
		expect(steamCdnUrl(570, "portrait")).toContain("library_600x900.jpg");
		// The local cache names the header asset differently from the CDN — pinned because it is
		// the single most common way to get Steam art wrong.
		expect(steamCdnUrl(570, "header")).not.toContain("library_header");
		expect(steamCdnUrl(0x8000_0001, "header")).toBeUndefined();
	});

	test("grid filenames follow Steam's per-kind naming", () => {
		expect(gridFilenames(570, "portrait")).toEqual(["570p.png", "570p.jpg"]);
		expect(gridFilenames(570, "hero")).toEqual(["570_hero.png", "570_hero.jpg"]);
		expect(gridFilenames(570, "logo")).toEqual(["570_logo.png", "570_logo.jpg"]);
		expect(gridFilenames(570, "header")).toEqual(["570.png", "570.jpg"]);
	});

	test("finds cached and user-override art on disk", () => {
		const dir = tmp("art");
		const hashDir = path.join(dir, "appcache", "librarycache", "570", "abc123");
		fs.mkdirSync(hashDir, { recursive: true });
		fs.writeFileSync(path.join(hashDir, "library_600x900.jpg"), "x");
		expect(findLocalArtFile(dir, 570, "portrait")).toBe(
			path.join(hashDir, "library_600x900.jpg"),
		);
		expect(findLocalArtFile(dir, 570, "hero")).toBeUndefined();

		const cfg = path.join(dir, "userdata", "1", "config");
		fs.mkdirSync(path.join(cfg, "grid"), { recursive: true });
		fs.writeFileSync(path.join(cfg, "grid", "570p.jpg"), "x");
		expect(findGridArtFile(cfg, 570, "portrait")).toBe(
			path.join(cfg, "grid", "570p.jpg"),
		);
		fs.rmSync(dir, { recursive: true, force: true });
	});

	test("fileUrl produces the host's local-art contract shape", () => {
		const u = fileUrl(path.join(path.sep, "home", "u", "My Games", "c.jpg"));
		expect(u.startsWith("file:///")).toBe(true);
		// Spaces are percent-encoded; the separators survive so the host can rebuild the path.
		expect(u).toContain("My%20Games");
		expect(u).toContain("/c.jpg");
	});
});

describe("reg.exe output", () => {
	test("parses value rows and leaves the key header alone", () => {
		const stdout = [
			"",
			"HKEY_LOCAL_MACHINE\\SOFTWARE\\WOW6432Node\\Valve\\Steam",
			"    InstallPath    REG_SZ    C:\\Program Files (x86)\\Steam",
			"    Language    REG_SZ    english",
			"",
		].join("\r\n");
		expect(parseRegQuery(stdout)).toEqual([
			{
				name: "InstallPath",
				type: "REG_SZ",
				// Data may contain spaces — only the first two columns are split off.
				data: "C:\\Program Files (x86)\\Steam",
			},
			{ name: "Language", type: "REG_SZ", data: "english" },
		]);
	});
});
