import type { Meta, StoryObj } from "@storybook/react-vite";
import { HostView } from "@/sections/Host/view";
import { compositors, hostInfo } from "./lib/fixtures";
import { Routed } from "./lib/routed";

const meta = {
	title: "Pages/Host",
	component: HostView,
	decorators: [
		(Story) => (
			<Routed>
				<Story />
			</Routed>
		),
	],
	args: {
		host: { data: hostInfo, isLoading: false, error: null },
		compositors: { data: compositors, isLoading: false, error: null },
	},
} satisfies Meta<typeof HostView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};

/** A non-Linux host has no compositor backends at all, so the card is absent rather than
 * reporting "none" at a card's worth of height (design/web-console-overhaul.md §3.3). */
export const NoCompositors: Story = {
	args: { compositors: { data: [], isLoading: false, error: null } },
};

/** A Windows host wears the Windows mark (and, correctly, shows no Compositors card). */
export const WindowsHost: Story = {
	args: {
		host: {
			data: { ...hostInfo, os: "windows", os_name: "Windows" },
			isLoading: false,
			error: null,
		},
		compositors: { data: [], isLoading: false, error: null },
	},
};

/** A gaming distro wears its OWN mark, not its family's: `cachyos` is resolved before the
 * `arch` it descends from, which is the whole point of shipping art for the leaf. */
export const CachyOsHost: Story = {
	args: {
		host: {
			data: { ...hostInfo, os: "linux/arch/cachyos", os_name: "CachyOS Linux" },
			isLoading: false,
			error: null,
		},
	},
};

/** An unrecognized distro chain walks up to its family mark — here neither `chimera` nor
 * `frontier` have art, so the icon degrades all the way to generic Tux. */
export const UnknownDistro: Story = {
	args: {
		host: {
			data: {
				...hostInfo,
				os: "linux/frontier/chimera",
				os_name: "Chimera Linux",
			},
			isLoading: false,
			error: null,
		},
	},
};

export const Loading: Story = {
	args: {
		host: { data: undefined, isLoading: true, error: null },
		compositors: { data: undefined, isLoading: true, error: null },
	},
};
