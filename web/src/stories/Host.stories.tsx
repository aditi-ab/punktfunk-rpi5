import type { Meta, StoryObj } from "@storybook/react-vite";
import { HostView } from "@/sections/Host/view";
import { compositors, hostInfo } from "./lib/fixtures";

const meta = {
	title: "Pages/Host",
	component: HostView,
	args: {
		host: { data: hostInfo, isLoading: false, error: null },
		compositors: { data: compositors, isLoading: false, error: null },
	},
} satisfies Meta<typeof HostView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {};

export const Loading: Story = {
	args: {
		host: { data: undefined, isLoading: true, error: null },
		compositors: { data: undefined, isLoading: true, error: null },
	},
};
