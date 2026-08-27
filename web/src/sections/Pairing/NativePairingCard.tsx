import { useQueryClient } from "@tanstack/react-query";
import { KeyRound, Smartphone, Timer } from "lucide-react";
import { type FC, useEffect, useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import type { ArmNativePairing } from "@/api/gen/model/armNativePairing";
import type { NativePairStatus } from "@/api/gen/model/nativePairStatus";
import {
	getGetNativePairingQueryKey,
	getListNativeClientsQueryKey,
	useDisarmNativePairing,
	useGetNativePairing,
} from "@/api/gen/native/native";
import { useArmNativePairing } from "@/api/pairing";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import {
	type AccessDraft,
	AccessControls,
	draftExpirySecs,
	GRANT_ALL,
} from "./access";

/** Seconds → `m:ss`. */
function fmtTime(secs: number): string {
	const s = Math.max(0, Math.floor(secs));
	return `${Math.floor(s / 60)}:${(s % 60).toString().padStart(2, "0")}`;
}

/**
 * Container: native (punktfunk/1) pairing — arm a window, poll fast while armed
 * for the live countdown, slow otherwise.
 */
export const NativePairingSection: FC = () => {
	const qc = useQueryClient();
	const native = useGetNativePairing({
		query: { refetchInterval: (q) => (q.state.data?.armed ? 1_000 : 4_000) },
	});
	const arm = useArmNativePairing();
	const disarm = useDisarmNativePairing();

	// A device pairs via the QUIC PIN ceremony, NOT through approve/deny, so nothing else
	// invalidates the paired-devices list on the happy path — it would stay stale until remount.
	// The status poll's `paired_clients` count is the pairing signal: when it rises, refresh the
	// list so the newly paired device appears immediately.
	const pairedCount = native.data?.paired_clients;
	const prevPairedCount = useRef(pairedCount);
	useEffect(() => {
		if (
			prevPairedCount.current !== undefined &&
			pairedCount !== undefined &&
			pairedCount !== prevPairedCount.current
		) {
			qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
		}
		prevPairedCount.current = pairedCount;
	}, [pairedCount, qc]);

	const refresh = () =>
		qc.invalidateQueries({ queryKey: getGetNativePairingQueryKey() });
	// The armed PIN lives HERE, not in the polled status: the BFF strips it from that read
	// (server/routes/api/v1/native/pair.get.ts) so a session cookie alone cannot learn one, which
	// leaves the password-gated arm response as the console's only copy. A reload therefore loses
	// it and the card falls back to its arm form, where re-arming mints a fresh PIN.
	const [pin, setPin] = useState<string | null>(null);
	const [wrongPassword, setWrongPassword] = useState(false);
	// `access` carries the window's device-access choice (grants + expiry) — NOT the window TTL;
	// whichever device completes this window's ceremony gets it.
	const onArm = (access: Partial<ArmNativePairing>, password: string) => {
		setWrongPassword(false);
		arm.mutate(
			{ ttl_secs: 120, ...access, password },
			{
				onSuccess: (status) => {
					setPin(status.pin ?? null);
					refresh();
				},
				onError: (e) => {
					if (e instanceof ApiError && e.status === 401) setWrongPassword(true);
				},
			},
		);
	};
	const onDisarm = () =>
		disarm.mutate(undefined, {
			onSuccess: () => {
				setPin(null);
				refresh();
			},
		});

	return (
		<NativePairingCard
			status={native}
			pin={pin}
			onArm={onArm}
			onDisarm={onDisarm}
			isArming={arm.isPending}
			wrongPassword={wrongPassword}
			isDisarming={disarm.isPending}
		/>
	);
};

/** Native (punktfunk/1) pairing: arm a window → DISPLAY the PIN the user enters on their device. */
export const NativePairingCard: FC<{
	status: Loadable<NativePairStatus>;
	/** The PIN of the window THIS console armed, or null — never from the polled status. */
	pin: string | null;
	/** Arm, carrying the chosen device access (empty = today's full/permanent behavior) and the
	 * console password the BFF re-verifies. */
	onArm: (access: Partial<ArmNativePairing>, password: string) => void;
	onDisarm: () => void;
	isArming: boolean;
	/** The last arm was refused: the password was wrong. */
	wrongPassword: boolean;
	isDisarming: boolean;
}> = ({
	status,
	pin,
	onArm,
	onDisarm,
	isArming,
	wrongPassword,
	isDisarming,
}) => {
	const d = status.data;
	// What the pairing device will be allowed to do — same defaults as the approve dialog (D1):
	// Full · Forever, because arming for your OWN next device is the common case.
	const [draft, setDraft] = useState<AccessDraft>({
		grants: GRANT_ALL,
		expiry: "forever",
		customHours: 4,
	});
	const [password, setPassword] = useState("");
	const arm = () => {
		const secs = draftExpirySecs(draft);
		const access: Partial<ArmNativePairing> = {};
		// The untouched Full · Forever default is omitted entirely: a re-pairing device then keeps
		// the access it already has (the API's omitted-fields contract), and an older host that
		// predates the fields sees exactly yesterday's request.
		if (draft.grants !== GRANT_ALL || secs != null) {
			access.grants = draft.grants;
			if (secs != null) access.expires_in_secs = secs;
		}
		onArm(access, password);
	};
	return (
		<QueryState
			isLoading={status.isLoading}
			error={status.error}
			refetch={status.refetch}
		>
			<Card>
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
					) : d.armed && pin ? (
						<div className="space-y-3">
							<p className="text-sm">{m.pairing_native_enter()}</p>
							<div className="rounded-lg border bg-muted/40 py-5 text-center font-mono text-4xl font-semibold tracking-[0.3em]">
								{pin}
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
							{/* The window's device-access choice — whichever device completes the
							    ceremony gets exactly this (design §6.2). */}
							<AccessControls
								value={draft}
								onChange={setDraft}
								idPrefix="arm"
							/>
							{/* Whoever completes this window's ceremony gets keyboard and mouse on this
							    machine, so arming re-confirms the console password — the BFF verifies and
							    strips it (util/confirm.ts). */}
							<div className="space-y-2">
								<Label htmlFor="arm-password">{m.store_spec_password()}</Label>
								<Input
									id="arm-password"
									type="password"
									autoComplete="current-password"
									value={password}
									onChange={(e) => setPassword(e.target.value)}
								/>
								<p className="text-xs text-muted-foreground">
									{m.pairing_password_help()}
								</p>
								{wrongPassword && (
									<p role="alert" className="text-xs text-destructive">
										{m.update_apply_wrong_password()}
									</p>
								)}
							</div>
							<Button
								disabled={isArming || password.length === 0}
								onClick={arm}
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
};
