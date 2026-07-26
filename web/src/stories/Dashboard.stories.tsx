import type { Meta, StoryObj } from "@storybook/react-vite";
import { DashboardView } from "@/sections/Dashboard/view";
import { statusActive, statusGrace, statusIdle } from "./lib/fixtures";

const meta = {
	title: "Pages/Dashboard",
	component: DashboardView,
	args: {
		onStopSession: () => {},
		onRequestIdr: () => {},
		onEndGame: () => {},
		isStopping: false,
		isRequestingIdr: false,
		isEndingGame: false,
	},
} satisfies Meta<typeof DashboardView>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ActiveSession: Story = {
	args: { status: { data: statusActive, isLoading: false, error: null } },
};

export const Idle: Story = {
	args: { status: { data: statusIdle, isLoading: false, error: null } },
};

/** A game whose client vanished: the host closes it when the countdown runs out. */
export const GameWaitingForItsClient: Story = {
	args: { status: { data: statusGrace, isLoading: false, error: null } },
};
