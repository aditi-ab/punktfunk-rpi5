import type { Meta, StoryObj } from "@storybook/react-vite";
import { PairingView } from "@/sections/Pairing/view";
import {
	nativeClients,
	nativePairArmed,
	pairingIdle,
	pendingDevices,
} from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };

const meta = {
	title: "Pages/Pairing",
	component: PairingView,
	parameters: { layout: "padded" },
	args: {
		onApprove: noop,
		onDeny: noop,
		pendingBusy: false,
		onArm: noop,
		onDisarm: noop,
		isArming: false,
		isDisarming: false,
		onUnpair: noop,
		isUnpairing: false,
		pin: "",
		onPinChange: noop,
		onSubmitPin: noop,
		isSubmittingPin: false,
		pinSuccess: false,
		pinError: false,
	},
} satisfies Meta<typeof PairingView>;

export default meta;
type Story = StoryObj<typeof meta>;

// The marketing state: a PIN armed for a phone, one device knocking for delegated
// approval, two already-paired native clients.
export const Armed: Story = {
	args: {
		pending: { data: pendingDevices, ...idle },
		native: { data: nativePairArmed, ...idle },
		clients: { data: nativeClients, ...idle },
		moonlight: { data: pairingIdle, ...idle },
	},
};
