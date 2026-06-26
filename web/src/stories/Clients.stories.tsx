import type { Meta, StoryObj } from "@storybook/react-vite";
import { ClientsView } from "@/sections/Clients/view";
import { pairedClients } from "./lib/fixtures";

const meta = {
	title: "Pages/Clients",
	component: ClientsView,
	args: { onUnpair: () => {}, isUnpairing: false },
} satisfies Meta<typeof ClientsView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Paired: Story = {
	args: { clients: { data: pairedClients, isLoading: false, error: null } },
};

export const Empty: Story = {
	args: { clients: { data: [], isLoading: false, error: null } },
};
