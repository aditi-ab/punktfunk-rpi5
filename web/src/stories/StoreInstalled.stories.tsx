import type { Meta, StoryObj } from "@storybook/react-vite";
import type { InstalledPlugin } from "@/api/store";
import { InstalledList } from "@/sections/Store/Installed";

// The installed-plugins list, driven straight from fixtures — it fetches nothing, so the header's
// "Update all" affordance can be checked in every state it has (absent, offered, and disabled
// because a run is already working through the queue) without a host or a catalog.

const meta = {
	title: "Store/InstalledList",
	component: InstalledList,
	parameters: { layout: "padded" },
	args: {
		onUpdate: () => {},
		onUpdateAll: () => {},
		onUninstall: () => {},
		busyPkg: null,
		batchRunning: false,
	},
} satisfies Meta<typeof InstalledList>;

export default meta;
type Story = StoryObj<typeof meta>;

const ROWS: InstalledPlugin[] = [
	{
		pkg: "@punktfunk/plugin-rom-manager",
		title: "ROM Manager",
		version: "0.3.1",
		tier: "verified",
		source: "unom official",
		entry_id: "rom-manager",
		running: true,
		update_available: "0.3.2",
	},
	{
		pkg: "@punktfunk/plugin-playnite",
		title: "Playnite",
		version: "0.2.0",
		tier: "external",
		source: "community catalog",
		entry_id: "playnite",
		running: true,
		update_available: "0.2.1",
	},
	{
		pkg: "@somebody/plugin-scratch",
		version: "0.1.0",
		tier: "unverified",
		running: false,
	},
];

const loaded = (data: InstalledPlugin[]) => ({
	data,
	isLoading: false,
	error: null,
});

/** Nothing to update: the header carries its title alone. */
export const UpToDate: Story = {
	args: {
		installed: loaded(ROWS.map(({ update_available, ...p }) => p)),
		updateCount: 0,
	},
};

/** Two updates on offer — the bulk action appears beside the title. */
export const UpdatesAvailable: Story = {
	args: { installed: loaded(ROWS), updateCount: 2 },
};

/** A run is working through the queue: every action here waits for it, bulk included. */
export const RunInFlight: Story = {
	args: { installed: loaded(ROWS), updateCount: 2, batchRunning: true },
};

/** One plugin's own uninstall is in flight — only that row's actions go quiet. */
export const RowBusy: Story = {
	args: {
		installed: loaded(ROWS),
		updateCount: 2,
		busyPkg: "@punktfunk/plugin-playnite",
	},
};

export const Empty: Story = { args: { installed: loaded([]), updateCount: 0 } };
