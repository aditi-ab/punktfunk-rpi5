import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Trash2 } from "lucide-react";
import type { FC } from "react";
import {
	getListPairedClientsQueryKey,
	useListPairedClients,
	useUnpairAllClients,
	useUnpairClient,
} from "@/api/gen/clients/clients";
import {
	getListNativeClientsQueryKey,
	useListNativeClients,
	useUnpairAllNativeClients,
	useUnpairNativeClient,
} from "@/api/gen/native/native";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { m } from "@/paraglide/messages";

/** The two pairing protocols a device can be paired over. */
export type PairedProtocol = "native" | "moonlight";

/** One paired device, normalized across the native + Moonlight lists. */
export interface PairedRow {
	protocol: PairedProtocol;
	fingerprint: string;
	/** Native devices carry a name; Moonlight clients carry a cert subject; either may be empty. */
	name: string;
}

/**
 * Container: ALL paired devices in one list. Merges the native (punktfunk/1) clients and the
 * GameStream/Moonlight clients — two separate host endpoints — into a single table tagged by
 * protocol, and routes each unpair back to the right endpoint.
 */
export const PairedDevicesSection: FC = () => {
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	const native = useListNativeClients();
	const moonlight = useListPairedClients();
	const unpairNative = useUnpairNativeClient();
	const unpairMoonlight = useUnpairClient();
	const unpairAllNative = useUnpairAllNativeClients();
	const unpairAllMoonlight = useUnpairAllClients();

	const rows: PairedRow[] = [
		...(native.data ?? []).map(
			(c): PairedRow => ({
				protocol: "native",
				fingerprint: c.fingerprint,
				name: c.name,
			}),
		),
		...(moonlight.data ?? []).map(
			(c): PairedRow => ({
				protocol: "moonlight",
				fingerprint: c.fingerprint,
				name: c.subject ?? "",
			}),
		),
	];

	const onUnpair = async (protocol: PairedProtocol, fingerprint: string) => {
		const ok = await confirm({
			title: m.pairing_native_unpair_confirm(),
			description: m.pairing_native_unpair_body(),
			confirmLabel: m.action_unpair(),
			destructive: true,
		});
		if (!ok) return;
		if (protocol === "native") {
			unpairNative.mutate(
				{ fingerprint },
				{
					onSuccess: () =>
						qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() }),
				},
			);
		} else {
			unpairMoonlight.mutate(
				{ fingerprint },
				{
					onSuccess: () =>
						qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() }),
				},
			);
		}
	};

	/**
	 * Unpair EVERY device, in one confirmation.
	 *
	 * Two calls, not one per device: each plane owns a separate trust store behind its own
	 * collection DELETE, and each of those empties its store in a single persisted write host-side.
	 * Only the planes actually holding a row are called — the native endpoint answers 503 on a host
	 * built without it, which would otherwise report a failure for devices that were never there.
	 */
	const onUnpairAll = async () => {
		const ok = await confirm({
			title: m.pairing_native_unpair_all_confirm({ count: rows.length }),
			description: m.pairing_native_unpair_all_body(),
			confirmLabel: m.action_unpair_all(),
			destructive: true,
		});
		if (!ok) return;
		const calls: Promise<unknown>[] = [];
		if (rows.some((r) => r.protocol === "native")) {
			calls.push(unpairAllNative.mutateAsync());
		}
		if (rows.some((r) => r.protocol === "moonlight")) {
			calls.push(unpairAllMoonlight.mutateAsync());
		}
		// allSettled, not all: the two planes are independent, so one failing must neither cancel
		// the other nor throw past this handler.
		const settled = await Promise.allSettled(calls);
		qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
		qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() });
		if (settled.some((r) => r.status === "rejected")) {
			toast.error(m.pairing_native_unpair_all_failed());
		}
	};

	// The fingerprint of the row whose unpair is in flight (if any) — so only THAT row's button
	// disables, not every row's.
	const pendingFingerprint =
		(unpairNative.isPending
			? unpairNative.variables?.fingerprint
			: undefined) ??
		(unpairMoonlight.isPending
			? unpairMoonlight.variables?.fingerprint
			: undefined) ??
		null;

	// Derived, not state: the two bulk calls are launched together and awaited together, so their
	// pending flags cover the whole run without a gap in the middle to flicker through.
	const isUnpairingAll =
		unpairAllNative.isPending || unpairAllMoonlight.isPending;

	return (
		<PairedDevices
			rows={rows}
			isLoading={native.isLoading || moonlight.isLoading}
			error={native.error ?? moonlight.error}
			refetch={() => {
				native.refetch();
				moonlight.refetch();
			}}
			onUnpair={onUnpair}
			onUnpairAll={onUnpairAll}
			pendingFingerprint={pendingFingerprint}
			isUnpairingAll={isUnpairingAll}
		/>
	);
};

/** All paired devices (native + Moonlight) in one table, differentiated by a protocol badge. */
export const PairedDevices: FC<{
	rows: PairedRow[];
	isLoading: boolean;
	error: unknown;
	refetch: () => void;
	onUnpair: (protocol: PairedProtocol, fingerprint: string) => void;
	/** Unpair every row, behind one confirmation. */
	onUnpairAll: () => void;
	/** Fingerprint of the row whose unpair is in flight, or null — only that row disables. */
	pendingFingerprint: string | null;
	/** A bulk unpair is walking the list — every control in the card disables until it finishes. */
	isUnpairingAll: boolean;
}> = ({
	rows,
	isLoading,
	error,
	refetch,
	onUnpair,
	onUnpairAll,
	pendingFingerprint,
	isUnpairingAll,
}) => (
	<Card>
		{/* flex-row: CardHeader stacks by default, and this one carries a trailing action. */}
		<CardHeader className="flex-row items-center justify-between gap-4 space-y-0">
			<h2 className="text-lg font-medium">{m.pairing_native_devices()}</h2>
			{/* Nothing to unpair in bulk when the list is empty (or still loading) — an enabled
			    button there would open a confirmation reading "Unpair all 0 devices?". */}
			{rows.length > 0 && (
				<Button
					variant="destructive"
					size="sm"
					disabled={isUnpairingAll}
					onClick={onUnpairAll}
				>
					<Trash2 className="size-4" />
					{m.action_unpair_all()}
				</Button>
			)}
		</CardHeader>

		<CardContent>
			<QueryState isLoading={isLoading} error={error} refetch={refetch}>
				{rows.length === 0 ? (
					m.pairing_native_empty()
				) : (
					<Table>
						<TableHeader>
							<TableRow>
								<TableHead>{m.clients_name()}</TableHead>
								<TableHead>{m.pairing_protocol()}</TableHead>
								<TableHead>{m.clients_fingerprint()}</TableHead>
								<TableHead className="w-12" />
							</TableRow>
						</TableHeader>
						<TableBody>
							{rows.map((r) => (
								<TableRow key={`${r.protocol}:${r.fingerprint}`}>
									<TableCell className="font-medium">{r.name || "—"}</TableCell>
									<TableCell>
										<Badge
											variant={
												r.protocol === "native" ? "default" : "secondary"
											}
										>
											{r.protocol === "native"
												? m.pairing_protocol_native()
												: m.pairing_protocol_moonlight()}
										</Badge>
									</TableCell>
									<TableCell className="font-mono text-xs text-muted-foreground">
										{r.fingerprint.slice(0, 16)}…
									</TableCell>
									<TableCell>
										<Button
											variant="ghost"
											size="icon"
											aria-label={m.action_unpair()}
											disabled={
												isUnpairingAll || pendingFingerprint === r.fingerprint
											}
											onClick={() => onUnpair(r.protocol, r.fingerprint)}
										>
											<Trash2 className="size-4 text-destructive" />
										</Button>
									</TableCell>
								</TableRow>
							))}
						</TableBody>
					</Table>
				)}
			</QueryState>
		</CardContent>
	</Card>
);
