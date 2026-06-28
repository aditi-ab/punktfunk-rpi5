import type { Meta, StoryObj } from "@storybook/react-vite";
import { StatsView } from "@/sections/Stats/view";
import { captureDetail, captureMetas, statsStatusIdle } from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };

const meta = {
	title: "Pages/Stats",
	component: StatsView,
	parameters: { layout: "padded" },
	args: {
		onStart: noop,
		onStop: noop,
		onSelect: noop,
		onDownload: noop,
		onDelete: noop,
		isStarting: false,
		isStopping: false,
		isDeleting: false,
	},
} satisfies Meta<typeof StatsView>;

export default meta;
type Story = StoryObj<typeof meta>;

// A finished run open in the detail view: recordings table populated and the full
// graph set (latency stack · throughput · loss/FEC) rendered from a deterministic
// fixture series — no live host or capture needed.
export const Recording: Story = {
	args: {
		status: { data: statsStatusIdle, ...idle },
		live: { data: undefined, ...idle },
		recordings: { data: captureMetas, ...idle },
		detail: { data: captureDetail, ...idle },
		selectedId: captureMetas[0]?.id ?? null,
	},
};

export const Empty: Story = {
	args: {
		status: { data: statsStatusIdle, ...idle },
		live: { data: undefined, ...idle },
		recordings: { data: [], ...idle },
		detail: { data: undefined, ...idle },
		selectedId: null,
	},
};
