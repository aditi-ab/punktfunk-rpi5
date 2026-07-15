import type { Meta, StoryObj } from "@storybook/react-vite";
import { MoonlightPairing } from "@/sections/Pairing/MoonlightPairingCard";
import { NativePairingCard } from "@/sections/Pairing/NativePairingCard";
import { PairedDevices } from "@/sections/Pairing/PairedDevices";
import { PendingDevices } from "@/sections/Pairing/PendingDevices";
import { PairingView } from "@/sections/Pairing/view";
import {
	nativeClients,
	nativePairArmed,
	pairedClients,
	pairingIdle,
	pendingDevices,
} from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };

// Renders the REAL page layout (PairingView) — the same component index.tsx uses. The live page
// fills its slots with the self-contained containers; here we fill them with the pure cards + mock
// state, so there's no duplicated composition to drift.
const meta = {
	title: "Pages/Pairing",
	component: PairingView,
	parameters: { layout: "padded" },
} satisfies Meta<typeof PairingView>;

export default meta;
type Story = StoryObj<typeof meta>;

// The marketing state: one device knocking for delegated approval, a PIN armed for a phone, the
// consolidated paired-devices list (native + Moonlight), idle Moonlight pairing.
export const Armed: Story = {
	args: {
		pending: (
			<PendingDevices
				pending={{ data: pendingDevices, ...idle }}
				onApprove={noop}
				onDeny={noop}
				pendingId={null}
			/>
		),
		native: (
			<NativePairingCard
				status={{ data: nativePairArmed, ...idle }}
				onArm={noop}
				onDisarm={noop}
				isArming={false}
				isDisarming={false}
			/>
		),
		moonlight: (
			<MoonlightPairing
				pairing={{ data: pairingIdle, ...idle }}
				pin=""
				onPinChange={noop}
				onSubmit={noop}
				isSubmitting={false}
				isSuccess={false}
				isError={false}
			/>
		),
		paired: (
			<PairedDevices
				rows={[
					...nativeClients.map((c) => ({
						protocol: "native" as const,
						fingerprint: c.fingerprint,
						name: c.name,
					})),
					...pairedClients.map((c) => ({
						protocol: "moonlight" as const,
						fingerprint: c.fingerprint,
						name: c.subject ?? "",
					})),
				]}
				isLoading={false}
				error={null}
				refetch={noop}
				onUnpair={noop}
				pendingFingerprint={null}
			/>
		),
	},
};
