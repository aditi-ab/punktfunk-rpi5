import { useQueryClient } from "@tanstack/react-query";
import {
	CheckCircle2,
	KeyRound,
	Smartphone,
	Timer,
	Trash2,
	UserPlus,
	X,
} from "lucide-react";
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
import { QueryState } from "@/components/query-state";
import { Section } from "@/components/section";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";

/** Seconds → `m:ss`. */
function fmtTime(secs: number): string {
	const s = Math.max(0, Math.floor(secs));
	return `${Math.floor(s / 60)}:${(s % 60).toString().padStart(2, "0")}`;
}

// Pairing composes four independent sub-cards, each its own little container
// (own query + mutations). They share the page's staggered entrance via <Section>.
export const SectionPairing: FC = () => {
	useLocale();
	return (
		<Section>
			<h1 className="text-2xl font-semibold">{m.pairing_title()}</h1>
			<PendingDevices />
			<NativePairingCard />
			<NativeDevices />
			<MoonlightPairingCard />
		</Section>
	);
};

/** Seconds since a knock → a short relative label. */
function fmtAge(secs: number): string {
	if (secs < 10) return m.pairing_pending_age_just_now();
	if (secs < 60) return m.pairing_pending_age_secs({ s: Math.floor(secs) });
	return m.pairing_pending_age_mins({ min: Math.floor(secs / 60) });
}

/**
 * Devices awaiting delegated approval: an unpaired device that tried to connect
 * shows up here, and Approve pairs it on the spot — no PIN fetched out of band.
 * Renders nothing while empty (the common case); polls so a knock appears while
 * the operator is looking at the page.
 */
function PendingDevices() {
	const qc = useQueryClient();
	const pending = useListPendingDevices({ query: { refetchInterval: 3_000 } });
	const approve = useApprovePendingDevice();
	const deny = useDenyPendingDevice();
	const rows = pending.data ?? [];
	// Stay out of the way when there's nothing pending and the fetch is healthy — but DON'T swallow
	// a real error (a 500 etc.); fall through to QueryState below so it surfaces like every other
	// section. (A 401 is handled globally by the fetcher's redirect-to-login.)
	if (rows.length === 0 && !pending.error) return null;

	const refresh = () => {
		qc.invalidateQueries({ queryKey: getListPendingDevicesQueryKey() });
		qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
	};
	const onApprove = (id: number, currentName: string) => {
		const name = prompt(m.pairing_pending_name_prompt(), currentName);
		if (name == null) return; // operator cancelled
		approve.mutate(
			{ id, data: { name: name.trim() ? name.trim() : null } },
			{ onSuccess: refresh },
		);
	};

	return (
		<div className="space-y-2">
			<h2 className="flex items-center gap-2 text-lg font-medium">
				<UserPlus className="size-4" />
				{m.pairing_pending_title()}
			</h2>
			<p className="text-sm text-muted-foreground">
				{m.pairing_pending_desc()}
			</p>
			<QueryState
				isLoading={pending.isLoading}
				error={pending.error}
				refetch={pending.refetch}
			>
				<Card>
					<CardContent className="p-0">
						<Table>
							<TableBody>
								{rows.map((p) => (
									<TableRow key={p.id}>
										<TableCell className="font-medium">{p.name}</TableCell>
										<TableCell className="font-mono text-xs text-muted-foreground">
											{p.fingerprint.slice(0, 16)}…
										</TableCell>
										<TableCell className="text-xs text-muted-foreground">
											{fmtAge(p.age_secs)}
										</TableCell>
										<TableCell className="text-right">
											<div className="flex justify-end gap-2">
												<Button
													size="sm"
													disabled={approve.isPending || deny.isPending}
													onClick={() => onApprove(p.id, p.name)}
												>
													{m.pairing_pending_approve()}
												</Button>
												<Button
													size="sm"
													variant="ghost"
													aria-label={m.pairing_pending_deny()}
													disabled={approve.isPending || deny.isPending}
													onClick={() =>
														deny.mutate({ id: p.id }, { onSuccess: refresh })
													}
												>
													<X className="size-4" />
												</Button>
											</div>
										</TableCell>
									</TableRow>
								))}
							</TableBody>
						</Table>
					</CardContent>
				</Card>
			</QueryState>
		</div>
	);
}

/** Native (punktfunk/1) pairing: arm a window → DISPLAY the PIN the user enters on their device. */
function NativePairingCard() {
	const qc = useQueryClient();
	// Poll fast while armed (live countdown), slow otherwise.
	const status = useGetNativePairing({
		query: { refetchInterval: (q) => (q.state.data?.armed ? 1_000 : 4_000) },
	});
	const arm = useArmNativePairing();
	const disarm = useDisarmNativePairing();
	const d = status.data;
	const refresh = () =>
		qc.invalidateQueries({ queryKey: getGetNativePairingQueryKey() });

	return (
		<QueryState
			isLoading={status.isLoading}
			error={status.error}
			refetch={status.refetch}
		>
			<Card className="max-w-md">
				<CardHeader>
					<CardTitle className="flex items-center gap-2">
						<Smartphone className="size-4" />
						{m.pairing_native_title()}
					</CardTitle>
				</CardHeader>
				<CardContent className="space-y-4">
					{!d?.enabled ? (
						<p className="text-sm text-muted-foreground">
							{m.pairing_native_disabled()}
						</p>
					) : d.armed && d.pin ? (
						<div className="space-y-3">
							<p className="text-sm">{m.pairing_native_enter()}</p>
							<div className="rounded-lg border bg-muted/40 py-5 text-center font-mono text-4xl font-semibold tracking-[0.3em]">
								{d.pin}
							</div>
							{d.expires_in_secs != null && (
								<p className="flex items-center justify-center gap-1.5 text-sm text-muted-foreground">
									<Timer className="size-4" />
									{m.pairing_native_expires()} {fmtTime(d.expires_in_secs)}
								</p>
							)}
							<Button
								variant="outline"
								className="w-full"
								disabled={disarm.isPending}
								onClick={() => disarm.mutate(undefined, { onSuccess: refresh })}
							>
								{m.pairing_native_cancel()}
							</Button>
						</div>
					) : (
						<>
							<p className="text-sm text-muted-foreground">
								{m.pairing_native_desc()}
							</p>
							<Button
								disabled={arm.isPending}
								onClick={() =>
									arm.mutate(
										{ data: { ttl_secs: 120 } },
										{ onSuccess: refresh },
									)
								}
							>
								<KeyRound className="size-4" />
								{m.pairing_native_arm()}
							</Button>
						</>
					)}
				</CardContent>
			</Card>
		</QueryState>
	);
}

/** The paired native (punktfunk/1) devices, with unpair. */
function NativeDevices() {
	const qc = useQueryClient();
	const clients = useListNativeClients();
	const unpair = useUnpairNativeClient();
	const rows = clients.data ?? [];

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

	return (
		<div className="space-y-2">
			<h2 className="text-lg font-medium">{m.pairing_native_devices()}</h2>
			<QueryState
				isLoading={clients.isLoading}
				error={clients.error}
				refetch={clients.refetch}
			>
				{rows.length === 0 ? (
					<Card>
						<CardContent className="p-6 text-center text-sm text-muted-foreground">
							{m.pairing_native_empty()}
						</CardContent>
					</Card>
				) : (
					<Card>
						<CardContent className="p-0">
							<Table>
								<TableHeader>
									<TableRow>
										<TableHead>{m.clients_name()}</TableHead>
										<TableHead>{m.clients_fingerprint()}</TableHead>
										<TableHead className="w-12" />
									</TableRow>
								</TableHeader>
								<TableBody>
									{rows.map((c) => (
										<TableRow key={c.fingerprint}>
											<TableCell className="font-medium">
												{c.name || "—"}
											</TableCell>
											<TableCell className="font-mono text-xs text-muted-foreground">
												{c.fingerprint.slice(0, 16)}…
											</TableCell>
											<TableCell>
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.action_unpair()}
													disabled={unpair.isPending}
													onClick={() => onUnpair(c.fingerprint)}
												>
													<Trash2 className="size-4 text-destructive" />
												</Button>
											</TableCell>
										</TableRow>
									))}
								</TableBody>
							</Table>
						</CardContent>
					</Card>
				)}
			</QueryState>
		</div>
	);
}

/** GameStream/Moonlight pairing: the client shows a PIN, the operator submits it here. */
function MoonlightPairingCard() {
	const qc = useQueryClient();
	const [pin, setPin] = useState("");
	const pairing = useGetPairingStatus({ query: { refetchInterval: 2_000 } });
	const submit = useSubmitPairingPin();
	const pending = pairing.data?.pin_pending ?? false;

	const onSubmit = (e: React.FormEvent) => {
		e.preventDefault();
		submit.mutate(
			{ data: { pin } },
			{
				onSuccess: () => {
					setPin("");
					qc.invalidateQueries({ queryKey: getGetPairingStatusQueryKey() });
				},
			},
		);
	};

	return (
		<QueryState
			isLoading={pairing.isLoading}
			error={pairing.error}
			refetch={pairing.refetch}
		>
			<Card className="max-w-md">
				<CardHeader>
					<CardTitle className="flex items-center gap-2">
						<KeyRound className="size-4" />
						{m.pairing_moonlight_title()}
					</CardTitle>
				</CardHeader>
				<CardContent>
					{!pending ? (
						<p className="text-sm text-muted-foreground">{m.pairing_idle()}</p>
					) : (
						<form onSubmit={onSubmit} className="space-y-4">
							<p className="text-sm">{m.pairing_waiting()}</p>
							<div className="space-y-2">
								<Label htmlFor="pin">{m.pairing_pin_label()}</Label>
								<Input
									id="pin"
									inputMode="numeric"
									autoComplete="off"
									maxLength={8}
									value={pin}
									onChange={(e) => setPin(e.target.value.replace(/\D/g, ""))}
									placeholder="0000"
									className="font-mono text-lg tracking-widest"
								/>
							</div>
							<Button
								type="submit"
								disabled={pin.length < 4 || submit.isPending}
							>
								{m.pairing_submit()}
							</Button>
							{submit.isSuccess && (
								<p className="flex items-center gap-1.5 text-sm text-[var(--success)]">
									<CheckCircle2 className="size-4" />
									{m.pairing_success()}
								</p>
							)}
							{submit.isError && (
								<p className="text-sm text-destructive">{m.pairing_failed()}</p>
							)}
						</form>
					)}
				</CardContent>
			</Card>
		</QueryState>
	);
}
