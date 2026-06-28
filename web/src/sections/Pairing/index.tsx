import { useQueryClient } from "@tanstack/react-query";
import { type FC, useState } from "react";
import {
	getGetNativePairingQueryKey,
	getListNativeClientsQueryKey,
	getListPendingDevicesQueryKey,
	useApprovePendingDevice,
	useArmNativePairing,
	useDenyPendingDevice,
	useDisarmNativePairing,
	useGetNativePairing,
	useListNativeClients,
	useListPendingDevices,
	useUnpairNativeClient,
} from "@/api/gen/native/native";
import {
	getGetPairingStatusQueryKey,
	useGetPairingStatus,
	useSubmitPairingPin,
} from "@/api/gen/pairing/pairing";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { PairingView } from "./view";

// Container: owns the four sub-cards' queries + mutations and hands a plain props
// surface to PairingView. (The presentational split mirrors Dashboard/Clients/Stats
// and lets Storybook render the page with mock state — no live host.)
export const SectionPairing: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const [pin, setPin] = useState("");

	// Devices awaiting delegated approval — polls so a knock appears while looking.
	const pending = useListPendingDevices({ query: { refetchInterval: 3_000 } });
	const approve = useApprovePendingDevice();
	const deny = useDenyPendingDevice();

	// Native (punktfunk/1) pairing: poll fast while armed (live countdown), slow otherwise.
	const native = useGetNativePairing({
		query: { refetchInterval: (q) => (q.state.data?.armed ? 1_000 : 4_000) },
	});
	const arm = useArmNativePairing();
	const disarm = useDisarmNativePairing();

	const clients = useListNativeClients();
	const unpair = useUnpairNativeClient();

	const pairing = useGetPairingStatus({ query: { refetchInterval: 2_000 } });
	const submit = useSubmitPairingPin();

	const refreshPending = () => {
		qc.invalidateQueries({ queryKey: getListPendingDevicesQueryKey() });
		qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
	};
	const refreshNative = () =>
		qc.invalidateQueries({ queryKey: getGetNativePairingQueryKey() });

	const onApprove = (id: number, currentName: string) => {
		const name = prompt(m.pairing_pending_name_prompt(), currentName);
		if (name == null) return; // operator cancelled
		approve.mutate(
			{ id, data: { name: name.trim() ? name.trim() : null } },
			{ onSuccess: refreshPending },
		);
	};
	const onDeny = (id: number) =>
		deny.mutate({ id }, { onSuccess: refreshPending });

	const onArm = () =>
		arm.mutate({ data: { ttl_secs: 120 } }, { onSuccess: refreshNative });
	const onDisarm = () => disarm.mutate(undefined, { onSuccess: refreshNative });

	const onUnpair = (fingerprint: string) => {
		if (!confirm(m.pairing_native_unpair_confirm())) return;
		unpair.mutate(
			{ fingerprint },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() }),
			},
		);
	};

	const onSubmitPin = () =>
		submit.mutate(
			{ data: { pin } },
			{
				onSuccess: () => {
					setPin("");
					qc.invalidateQueries({ queryKey: getGetPairingStatusQueryKey() });
				},
			},
		);

	return (
		<PairingView
			pending={pending}
			onApprove={onApprove}
			onDeny={onDeny}
			pendingBusy={approve.isPending || deny.isPending}
			native={native}
			onArm={onArm}
			onDisarm={onDisarm}
			isArming={arm.isPending}
			isDisarming={disarm.isPending}
			clients={clients}
			onUnpair={onUnpair}
			isUnpairing={unpair.isPending}
			moonlight={pairing}
			pin={pin}
			onPinChange={setPin}
			onSubmitPin={onSubmitPin}
			isSubmittingPin={submit.isPending}
			pinSuccess={submit.isSuccess}
			pinError={submit.isError}
		/>
	);
};
