import type { Meta, StoryObj } from "@storybook/react-vite";
import type { LogEntry } from "@/api/gen/model/logEntry";
import { LogsCard } from "@/sections/Logs/LogsCard";
import { LogsView } from "@/sections/Logs/view";

const noop = () => {};

// A deterministic slice of host logs covering every level, incl. the gamepad-driver health lines
// the page exists to surface — no live host needed.
const BASE = 1_750_000_000_000;
const entry = (
	seq: number,
	level: string,
	target: string,
	msg: string,
): LogEntry => ({ seq, ts_ms: BASE + seq * 750, level, target, msg });

const fixtureEntries: LogEntry[] = [
	entry(1, "INFO", "punktfunk_host", "punktfunk-host 0.4.2 (punktfunk_core ABI v2)"),
	entry(2, "INFO", "punktfunk_host::mgmt", "management API listening over HTTPS addr=0.0.0.0:47990"),
	entry(3, "DEBUG", "punktfunk_host::discovery", "mDNS advertise _punktfunk._udp pair=required"),
	entry(4, "INFO", "punktfunk_host::punktfunk1", "session start mode=1920x1080@60 codec=hevc"),
	entry(5, "INFO", "punktfunk_host::inject", "virtual Xbox 360 created (Windows XUSB companion)"),
	entry(6, "WARN", "punktfunk_host::inject", "gamepad driver not attached to Global\\pfxusb-shm-0 after 3s — is the pf_xusb driver installed? (punktfunk-host.exe driver install --gamepad)"),
	entry(7, "ERROR", "punktfunk_host::inject", "virtual Xbox 360 creation failed — controller input disabled (is the pf_xusb driver installed?)"),
	entry(8, "INFO", "punktfunk_host::encode", "NVENC opened 1920x1080 nv12 gop=inf rfi=on"),
];

const meta = {
	title: "Pages/Logs",
	component: LogsView,
	parameters: { layout: "padded" },
} satisfies Meta<typeof LogsView>;

export default meta;
type Story = StoryObj<typeof meta>;

// The real page layout (LogsView) with the pure viewer card + fixture entries in its slot.
export const Following: Story = {
	args: {
		viewer: (
			<LogsCard
				entries={fixtureEntries}
				follow
				onFollow={noop}
				onClear={noop}
				dropped={false}
			/>
		),
	},
};

export const PausedWithGap: Story = {
	args: {
		viewer: (
			<LogsCard
				entries={fixtureEntries}
				follow={false}
				onFollow={noop}
				onClear={noop}
				dropped
			/>
		),
	},
};
