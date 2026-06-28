import {
	CheckCircle2,
	KeyRound,
	Smartphone,
	Timer,
	Trash2,
	UserPlus,
	X,
} from "lucide-react";
import type { FC } from "react";
import type { NativeClient } from "@/api/gen/model/nativeClient";
import type { NativePairStatus } from "@/api/gen/model/nativePairStatus";
import type { PairingStatus } from "@/api/gen/model/pairingStatus";
import type { PendingDevice } from "@/api/gen/model/pendingDevice";
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
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

/** Seconds → `m:ss`. */
function fmtTime(secs: number): string {
	const s = Math.max(0, Math.floor(secs));
	return `${Math.floor(s / 60)}:${(s % 60).toString().padStart(2, "0")}`;
}

/** Seconds since a knock → a short relative label. */
function fmtAge(secs: number): string {
	if (secs < 10) return m.pairing_pending_age_just_now();
	if (secs < 60) return m.pairing_pending_age_secs({ s: Math.floor(secs) });
	return m.pairing_pending_age_mins({ min: Math.floor(secs / 60) });
}

export interface PairingViewProps {
	pending: Loadable<PendingDevice[]>;
	onApprove: (id: number, currentName: string) => void;
	onDeny: (id: number) => void;
	pendingBusy: boolean;

	native: Loadable<NativePairStatus>;
	onArm: () => void;
	onDisarm: () => void;
	isArming: boolean;
	isDisarming: boolean;

	clients: Loadable<NativeClient[]>;
	onUnpair: (fingerprint: string) => void;
	isUnpairing: boolean;

	moonlight: Loadable<PairingStatus>;
	pin: string;
	onPinChange: (v: string) => void;
	onSubmitPin: () => void;
	isSubmittingPin: boolean;
	pinSuccess: boolean;
	pinError: boolean;
}

// Pairing composes four independent sub-cards. This is the pure presentational
// surface (mirrors every other page's view.tsx); the container in index.tsx wires
// the queries + mutations. Stories feed mock state so no live host is needed.
export const PairingView: FC<PairingViewProps> = (props) => (
	<Section>
		<h1 className="text-2xl font-semibold">{m.pairing_title()}</h1>
		<PendingDevicesCard
			pending={props.pending}
			onApprove={props.onApprove}
			onDeny={props.onDeny}
			busy={props.pendingBusy}
		/>
		<NativePairingCard
			status={props.native}
			onArm={props.onArm}
			onDisarm={props.onDisarm}
			isArming={props.isArming}
			isDisarming={props.isDisarming}
		/>
		<NativeDevicesCard
			clients={props.clients}
			onUnpair={props.onUnpair}
			isUnpairing={props.isUnpairing}
		/>
		<MoonlightPairingCard
			pairing={props.moonlight}
			pin={props.pin}
			onPinChange={props.onPinChange}
			onSubmit={props.onSubmitPin}
			isSubmitting={props.isSubmittingPin}
			isSuccess={props.pinSuccess}
			isError={props.pinError}
		/>
	</Section>
);

/**
 * Devices awaiting delegated approval: an unpaired device that tried to connect
 * shows up here, and Approve pairs it on the spot. Renders nothing while empty
 * (the common case) unless there's an error to surface.
 */
const PendingDevicesCard: FC<{
	pending: Loadable<PendingDevice[]>;
	onApprove: (id: number, currentName: string) => void;
	onDeny: (id: number) => void;
	busy: boolean;
}> = ({ pending, onApprove, onDeny, busy }) => {
	const rows = pending.data ?? [];
	// Stay out of the way when there's nothing pending and the fetch is healthy — but DON'T swallow
	// a real error (a 500 etc.); fall through to QueryState below so it surfaces like every other
	// section. (A 401 is handled globally by the fetcher's redirect-to-login.)
	if (rows.length === 0 && !pending.error) return null;

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
													disabled={busy}
													onClick={() => onApprove(p.id, p.name)}
												>
													{m.pairing_pending_approve()}
												</Button>
												<Button
													size="sm"
													variant="ghost"
													aria-label={m.pairing_pending_deny()}
													disabled={busy}
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
					</CardContent>
				</Card>
			</QueryState>
		</div>
	);
};

/** Native (punktfunk/1) pairing: arm a window → DISPLAY the PIN the user enters on their device. */
const NativePairingCard: FC<{
	status: Loadable<NativePairStatus>;
	onArm: () => void;
	onDisarm: () => void;
	isArming: boolean;
	isDisarming: boolean;
}> = ({ status, onArm, onDisarm, isArming, isDisarming }) => {
	const d = status.data;
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
								disabled={isDisarming}
								onClick={onDisarm}
							>
								{m.pairing_native_cancel()}
							</Button>
						</div>
					) : (
						<>
							<p className="text-sm text-muted-foreground">
								{m.pairing_native_desc()}
							</p>
							<Button disabled={isArming} onClick={onArm}>
								<KeyRound className="size-4" />
								{m.pairing_native_arm()}
							</Button>
						</>
					)}
				</CardContent>
			</Card>
		</QueryState>
	);
};

/** The paired native (punktfunk/1) devices, with unpair. */
const NativeDevicesCard: FC<{
	clients: Loadable<NativeClient[]>;
	onUnpair: (fingerprint: string) => void;
	isUnpairing: boolean;
}> = ({ clients, onUnpair, isUnpairing }) => {
	const rows = clients.data ?? [];
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
													disabled={isUnpairing}
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
};

/** GameStream/Moonlight pairing: the client shows a PIN, the operator submits it here. */
const MoonlightPairingCard: FC<{
	pairing: Loadable<PairingStatus>;
	pin: string;
	onPinChange: (v: string) => void;
	onSubmit: () => void;
	isSubmitting: boolean;
	isSuccess: boolean;
	isError: boolean;
}> = ({
	pairing,
	pin,
	onPinChange,
	onSubmit,
	isSubmitting,
	isSuccess,
	isError,
}) => {
	const pending = pairing.data?.pin_pending ?? false;
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
						<form
							onSubmit={(e) => {
								e.preventDefault();
								onSubmit();
							}}
							className="space-y-4"
						>
							<p className="text-sm">{m.pairing_waiting()}</p>
							<div className="space-y-2">
								<Label htmlFor="pin">{m.pairing_pin_label()}</Label>
								<Input
									id="pin"
									inputMode="numeric"
									autoComplete="off"
									maxLength={8}
									value={pin}
									onChange={(e) =>
										onPinChange(e.target.value.replace(/\D/g, ""))
									}
									placeholder="0000"
									className="font-mono text-lg tracking-widest"
								/>
							</div>
							<Button type="submit" disabled={pin.length < 4 || isSubmitting}>
								{m.pairing_submit()}
							</Button>
							{isSuccess && (
								<p className="flex items-center gap-1.5 text-sm text-[var(--success)]">
									<CheckCircle2 className="size-4" />
									{m.pairing_success()}
								</p>
							)}
							{isError && (
								<p className="text-sm text-destructive">{m.pairing_failed()}</p>
							)}
						</form>
					)}
				</CardContent>
			</Card>
		</QueryState>
	);
};
