import { useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, KeyRound } from "lucide-react";
import { type FC, useState } from "react";
import type { PairingStatus } from "@/api/gen/model/pairingStatus";
import {
	getGetPairingStatusQueryKey,
	useGetPairingStatus,
	useSubmitPairingPin,
} from "@/api/gen/pairing/pairing";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

/** Container: GameStream/Moonlight pairing — poll status, own the PIN entry, submit it. */
export const MoonlightPairingSection: FC = () => {
	const qc = useQueryClient();
	const [pin, setPin] = useState("");
	const pairing = useGetPairingStatus({ query: { refetchInterval: 2_000 } });
	const submit = useSubmitPairingPin();

	const onSubmit = () =>
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
		<MoonlightPairing
			pairing={pairing}
			pin={pin}
			onPinChange={setPin}
			onSubmit={onSubmit}
			isSubmitting={submit.isPending}
			isSuccess={submit.isSuccess}
			isError={submit.isError}
		/>
	);
};

/** GameStream/Moonlight pairing: the client shows a PIN, the operator submits it here. */
export const MoonlightPairing: FC<{
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
			<Card>
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
