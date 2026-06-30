import { useQueryClient } from "@tanstack/react-query";
import { Trash2 } from "lucide-react";
import type { FC } from "react";
import {
	getListPairedClientsQueryKey,
	useListPairedClients,
	useUnpairClient,
} from "@/api/gen/clients/clients";
import {
	getListNativeClientsQueryKey,
	useListNativeClients,
	useUnpairNativeClient,
} from "@/api/gen/native/native";
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
	const native = useListNativeClients();
	const moonlight = useListPairedClients();
	const unpairNative = useUnpairNativeClient();
	const unpairMoonlight = useUnpairClient();

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

	const onUnpair = (protocol: PairedProtocol, fingerprint: string) => {
		if (!confirm(m.pairing_native_unpair_confirm())) return;
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
			isUnpairing={unpairNative.isPending || unpairMoonlight.isPending}
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
	isUnpairing: boolean;
}> = ({ rows, isLoading, error, refetch, onUnpair, isUnpairing }) => (
	<Card>
		<CardHeader>
			<h2 className="text-lg font-medium">{m.pairing_native_devices()}</h2>
		</CardHeader>

		<CardContent className="p-6">
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
											disabled={isUnpairing}
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
