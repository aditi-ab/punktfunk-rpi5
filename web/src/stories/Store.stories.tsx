import type { Meta, StoryObj } from "@storybook/react-vite";
import type { StoreEntry } from "@/api/store";
import { StoreCard } from "@/sections/Store/Browse";

// The store catalog card, rendered straight from a fixture entry — it fetches nothing, so
// the visuals (padding, platform labels, tier chip, blocked/incompatible states) can be
// checked without a host or a catalog.

const meta = {
	title: "Store/StoreCard",
	component: StoreCard,
	parameters: { layout: "centered" },
	args: { onInstall: () => {} },
} satisfies Meta<typeof StoreCard>;

export default meta;
type Story = StoryObj<typeof meta>;

const BASE: StoreEntry = {
	id: "rom-manager",
	pkg: "@punktfunk/plugin-rom-manager",
	title: "ROM Manager",
	description:
		"Scans your ROM directories, maps them to emulators, fetches box art, and reconciles everything into the host game library as a provider.",
	icon: "gamepad-2",
	author: "unom",
	homepage: "https://git.unom.io/unom/punktfunk-plugin-rom-manager",
	license: "MIT OR Apache-2.0",
	version: "0.3.2",
	source: "unom official",
	tier: "verified",
	// The catalog stores lowercase platform IDENTIFIERS; the card renders display names.
	platforms: ["linux", "windows"],
	compatible: true,
	update_available: false,
};

export const Default: Story = { args: { entry: BASE } };

export const AllPlatforms: Story = {
	args: { entry: { ...BASE, platforms: ["linux", "windows", "macos"] } },
};

export const SinglePlatform: Story = {
	args: {
		entry: {
			...BASE,
			id: "playnite",
			pkg: "@punktfunk/plugin-playnite",
			title: "Playnite",
			description:
				"Syncs your Playnite library into the host game library — every store and emulator Playnite manages, launched back through Playnite.",
			icon: "library-big",
			version: "0.2.0",
			platforms: ["windows"],
		},
	},
};

export const Installed: Story = {
	args: { entry: { ...BASE, installed_version: "0.3.2" } },
};

export const UpdateAvailable: Story = {
	args: {
		entry: { ...BASE, installed_version: "0.3.1", update_available: true },
	},
};

export const Incompatible: Story = {
	args: {
		entry: {
			...BASE,
			compatible: false,
			incompatible_reason:
				"needs punktfunk host ≥ 0.16.0 (this host is 0.15.0)",
		},
	},
};

export const Blocked: Story = {
	args: {
		entry: { ...BASE, blocked: "withdrawn by the author (CVE-2026-1234)" },
	},
};

export const External: Story = {
	args: {
		entry: { ...BASE, tier: "external", source: "community catalog" },
	},
};
