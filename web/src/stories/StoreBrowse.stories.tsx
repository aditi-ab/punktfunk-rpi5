import type { Meta, StoryObj } from "@storybook/react-vite";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { type StoreCatalog, type StoreEntry, storeKeys } from "@/api/store";
import { BrowseTab } from "@/sections/Store/Browse";

// The Browse tab over a fixture catalog: one heading per group, the group chips, and a launcher
// found on this host leading its group. The catalog query is seeded and never goes stale, so
// nothing fetches.

const entry = (
	id: string,
	title: string,
	categories: string[] | undefined,
	extra: Partial<StoreEntry> = {},
): StoreEntry => ({
	id,
	pkg: `@punktfunk/plugin-${id}`,
	title,
	description: `${title}, from the fixture catalog.`,
	icon: "gamepad-2",
	author: "unom",
	version: "0.1.0",
	source: "unom",
	tier: "verified",
	platforms: ["linux", "windows"],
	compatible: true,
	update_available: false,
	categories,
	...extra,
});

const CATALOG: StoreCatalog = {
	host: { version: "0.41.0", platform: "linux" },
	busy: false,
	sources: [],
	plugins: [
		entry("epic", "Epic Games", ["library"]),
		entry("heroic", "Heroic", ["library"], { detected: false }),
		entry("steam", "Steam", ["library"], { detected: true }),
		entry("lutris", "Lutris", ["library"], { installed_version: "0.2.0" }),
		entry("igdb", "IGDB", ["metadata"]),
		entry("steamgriddb", "SteamGridDB", ["metadata"]),
		entry("play-history", "Play history", ["tools"]),
		entry("stats-black-box", "Stats black box", ["tools"]),
		entry("legacy", "Legacy scanner", undefined, {
			tier: "external",
			source: "community",
		}),
	],
};

const meta = {
	title: "Store/Browse",
	component: BrowseTab,
	parameters: { layout: "padded" },
	args: { onInstall: () => {}, onInstallSpec: () => {} },
	decorators: [
		(Story) => {
			const qc = useQueryClient();
			useState(() => {
				qc.setQueryDefaults(storeKeys.catalog, { staleTime: Infinity });
				qc.setQueryData(storeKeys.catalog, CATALOG);
				return null;
			});
			return <Story />;
		},
	],
} satisfies Meta<typeof BrowseTab>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Grouped: Story = {};
