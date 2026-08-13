import type { Meta, StoryObj } from "@storybook/react-vite";
import type { PendingUpdate } from "@/api/store";
import { UpdateAllDialog } from "@/sections/Store/InstallDialogs";

// The one confirmation an "Update all" run takes. Rendered open, from fixtures, so the escalation
// rule can be read off the screen: all-verified is an ordinary confirm, and a single entry from an
// operator-added source turns the whole dialog amber and names the catalogs it came from.

const meta = {
	title: "Store/UpdateAllDialog",
	component: UpdateAllDialog,
	parameters: { layout: "fullscreen" },
	args: {
		skipped: [],
		isPending: false,
		onCancel: () => {},
		onConfirm: () => {},
	},
} satisfies Meta<typeof UpdateAllDialog>;

export default meta;
type Story = StoryObj<typeof meta>;

const update = (
	title: string,
	pkg: string,
	from: string,
	to: string,
	source = "unom official",
	tier: "verified" | "external" = "verified",
): PendingUpdate => ({
	plugin: {
		pkg,
		title,
		version: from,
		tier,
		source,
		running: true,
		update_available: to,
	},
	entry: {
		id: pkg.split("/").pop() ?? pkg,
		pkg,
		title,
		description: "",
		author: "unom",
		version: to,
		source,
		tier,
		platforms: ["linux", "windows"],
		compatible: true,
		update_available: true,
	},
});

const VERIFIED: PendingUpdate[] = [
	update("ROM Manager", "@punktfunk/plugin-rom-manager", "0.3.1", "0.3.2"),
	update("Steam Library", "@punktfunk/plugin-steam", "1.0.0", "1.1.0"),
];

/** Everything from the built-in catalog: a plain confirm, no warning to earn. */
export const AllVerified: Story = { args: { updates: VERIFIED } };

/** One entry from a catalog the operator added — the whole dialog escalates and names it. */
export const WithExternal: Story = {
	args: {
		updates: [
			...VERIFIED,
			update(
				"Playnite",
				"@punktfunk/plugin-playnite",
				"0.2.0",
				"0.2.1",
				"community catalog",
				"external",
			),
		],
	},
};

/** Updates the run will not attempt are named, so the button's count adds up on screen. */
export const WithSkipped: Story = {
	args: {
		updates: VERIFIED,
		skipped: ["Emulator Bridge", "@somebody/plugin-scratch"],
	},
};

/** A host with a lot installed: the list scrolls rather than pushing the footer off screen. */
export const LongList: Story = {
	args: {
		updates: Array.from({ length: 12 }, (_, i) =>
			update(
				`Plugin ${i + 1}`,
				`@punktfunk/plugin-number-${i + 1}`,
				`0.${i}.0`,
				`0.${i}.1`,
			),
		),
	},
};
