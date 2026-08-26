import { useQueryClient } from "@tanstack/react-query";
import { UserPlus, X } from "lucide-react";
import { type FC, useState } from "react";
import { ApiError } from "@/api/fetcher";
import type { ApprovePending } from "@/api/gen/model/approvePending";
import type { PendingDevice } from "@/api/gen/model/pendingDevice";
import {
	getListNativeClientsQueryKey,
	getListPendingDevicesQueryKey,
	useDenyPendingDevice,
	useListPendingDevices,
} from "@/api/gen/native/native";
import { useApprovePendingDevice } from "@/api/pairing";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableRow } from "@/components/ui/table";
import type { Loadable } from "@/lib/query";
import { fmtAge } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { ApproveDialog } from "./ApproveDialog";

/**
 * Container: devices awaiting delegated approval. Polls so a knock appears while
 * looking; approving pairs the device, so it also refreshes the paired-clients
 * list (owned by the PairedDevices subsection — invalidated here by query key).
 */
export const PendingDevicesSection: FC = () => {
	const qc = useQueryClient();
	// A knock arrives as a `pairing.pending` event (api/events.ts), so the timer is the fallback —
	// but it stays reasonably brisk: this list is the one the operator is actively waiting on, and
	// the rows carry an age that should not visibly lag.
	const pending = useListPendingDevices({ query: { refetchInterval: 10_000 } });
	const approve = useApprovePendingDevice();
	const deny = useDenyPendingDevice();
	// The row whose Approve dialog is open — a snapshot, so the 10 s poll can't reset the form.
	const [approving, setApproving] = useState<PendingDevice | null>(null);
	const [wrongPassword, setWrongPassword] = useState(false);

	const refresh = () => {
		qc.invalidateQueries({ queryKey: getListPendingDevicesQueryKey() });
		qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
	};
	const openApprove = (device: PendingDevice | null) => {
		setWrongPassword(false);
		setApproving(device);
	};
	const onApprove = (id: number, body: ApprovePending, password: string) => {
		setWrongPassword(false);
		approve.mutate(
			{ id, data: body, password },
			{
				onSuccess: () => {
					setApproving(null);
					refresh();
				},
				onError: (e) => {
					if (e instanceof ApiError && e.status === 401) setWrongPassword(true);
				},
			},
		);
	};
	const onDeny = (id: number) => deny.mutate({ id }, { onSuccess: refresh });

	// The id of the row whose approve/deny is in flight — only that row's buttons disable.
	const pendingId =
		(approve.isPending ? approve.variables?.id : undefined) ??
		(deny.isPending ? deny.variables?.id : undefined) ??
		null;

	return (
		<>
			<PendingDevices
				pending={pending}
				onApprove={openApprove}
				onDeny={onDeny}
				pendingId={pendingId}
			/>
			<ApproveDialog
				device={approving}
				onCancel={() => openApprove(null)}
				onApprove={onApprove}
				isPending={approve.isPending}
				wrongPassword={wrongPassword}
			/>
		</>
	);
};

/**
 * Devices awaiting delegated approval: an unpaired device that tried to connect
 * shows up here, and Approve opens the access dialog (name + access level +
 * expiry — one dialog, per design §6.1). Renders nothing while empty
 * (the common case) unless there's an error to surface.
 */
export const PendingDevices: FC<{
	pending: Loadable<PendingDevice[]>;
	/** Opens the approve dialog for this row. */
	onApprove: (device: PendingDevice) => void;
	onDeny: (id: number) => void;
	/** Id of the row whose approve/deny is in flight, or null — only that row disables. */
	pendingId: number | null;
}> = ({ pending, onApprove, onDeny, pendingId }) => {
	const rows = pending.data ?? [];
	// Stay out of the way when there's nothing pending and the fetch is healthy — but DON'T swallow
	// a real error (a 500 etc.); fall through to QueryState below so it surfaces like every other
	// section. (A 401 is handled globally by the fetcher's redirect-to-login.)
	if (rows.length === 0 && !pending.error) return null;

	return (
		<Card>
			<CardContent flush>
				<CardHeader>
					<CardTitle>
						<h2 className="flex items-center gap-2 text-lg font-medium">
							<UserPlus className="size-4" />
							{m.pairing_pending_title()}
						</h2>
						<p className="text-sm text-muted-foreground">
							{m.pairing_pending_desc()}
						</p>
					</CardTitle>
				</CardHeader>

				<QueryState
					isLoading={pending.isLoading}
					error={pending.error}
					refetch={pending.refetch}
				>
					<Table>
						<TableBody>
							{rows.map((p) => (
								<TableRow className="h-18" key={p.id}>
									{/* The row must keep the actions on-canvas in a portrait phone
									    viewport: the name flexes and truncates (w-full + max-w-0),
									    and the fingerprint/age columns collapse into a sub-line
									    here below md/sm instead of widening the row past the
									    screen (the table wrapper scrolls, the page doesn't — an
									    off-canvas Approve button is unreachable on mobile). */}
									<TableCell className="w-full max-w-0 font-medium">
										<div className="truncate">{p.name}</div>
										<div className="truncate font-mono text-xs font-normal text-muted-foreground md:hidden">
											{p.fingerprint.slice(0, 16)}…
											<span className="ml-2 font-sans sm:hidden">
												{fmtAge(p.age_secs)}
											</span>
										</div>
									</TableCell>
									<TableCell className="hidden font-mono text-xs text-muted-foreground md:table-cell">
										{p.fingerprint.slice(0, 16)}…
									</TableCell>
									<TableCell className="hidden text-xs text-muted-foreground sm:table-cell">
										{fmtAge(p.age_secs)}
									</TableCell>
									<TableCell className="whitespace-nowrap text-right">
										<div className="flex justify-end gap-2">
											<Button
												size="sm"
												disabled={pendingId === p.id}
												onClick={() => onApprove(p)}
											>
												{m.pairing_pending_approve()}
											</Button>
											<Button
												size="sm"
												variant="ghost"
												aria-label={m.pairing_pending_deny()}
												disabled={pendingId === p.id}
												onClick={() => onDeny(p.id)}
											>
												<X className="size-4" />
											</Button>
										</div>
									</TableCell>
								</TableRow>
							))}
						</TableBody>
					</Table>
				</QueryState>
			</CardContent>
		</Card>
	);
};
