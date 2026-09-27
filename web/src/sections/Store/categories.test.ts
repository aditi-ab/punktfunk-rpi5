import { describe, expect, test } from "bun:test";
import type { StoreEntry } from "@/api/store";
import { groupCatalog, groupOf } from "./categories";

const entry = (
	title: string,
	categories?: string[],
	detected?: boolean,
): StoreEntry => ({
	id: title.toLowerCase(),
	pkg: `@punktfunk/plugin-${title.toLowerCase()}`,
	title,
	description: "",
	author: "unom",
	version: "0.1.0",
	source: "unom",
	tier: "verified",
	platforms: [],
	compatible: true,
	update_available: false,
	categories,
	detected,
});

describe("store categories", () => {
	test("an entry lands in its first known group, else other", () => {
		expect(groupOf(entry("Steam", ["library"]))).toBe("library");
		expect(groupOf(entry("Odd", ["overlay", "tools"]))).toBe("tools");
		expect(groupOf(entry("Old"))).toBe("other");
		expect(groupOf(entry("New", ["overlay"]))).toBe("other");
	});

	test("groups keep page order, skip empty ones, and lift detected entries", () => {
		const groups = groupCatalog([
			entry("RTSS", ["tools"], false),
			entry("Epic", ["library"]),
			entry("Steam", ["library"], true),
			entry("Amazon", ["library"], false),
			entry("Legacy"),
		]);
		expect(groups.map(([g, list]) => [g, list.map((e) => e.title)])).toEqual([
			["library", ["Steam", "Amazon", "Epic"]],
			["tools", ["RTSS"]],
			["other", ["Legacy"]],
		]);
	});
});
