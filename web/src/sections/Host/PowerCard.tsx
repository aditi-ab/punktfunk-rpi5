import { Power } from "lucide-react";
import { type FC, useState } from "react";
import { ApiError, apiFetch } from "@/api/fetcher";
import { useListActions } from "@/api/gen/actions/actions";
import type { ActionInfo } from "@/api/gen/model";
import {
	PasswordConfirmField,
	usePasswordFailure,
} from "@/components/password-confirm";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { apiErrorMessage } from "@/lib/errors";
import { m } from "@/paraglide/messages";

/** Localized titles for the KNOWN action ids; unknown ids fall back to the server's title —
 * the contract that lets future host actions appear with no console release. */
export const actionTitle = (a: ActionInfo): string => {
	switch (a.id) {
		case "power.sleep":
			return m.host_power_sleep();
		case "power.reboot":
			return m.host_power_reboot();
		case "power.shutdown":
			return m.host_power_shutdown();
		case "host.restart":
			return m.host_power_restart_service();
		default:
			return a.title;
	}
};

/**
 * Host power (design/host-actions.md §7, the admin lane's free win — also the "no restart
 * route" gap the update design named): the discovered host actions as password-confirmed
 * buttons. Unavailable actions render disabled with the host's honest reason instead of
 * being hidden.
 */
export const PowerSection: FC = () => {
	const actions = useListActions();
	const [confirming, setConfirming] = useState<ActionInfo | null>(null);
	const [sent, setSent] = useState<string | null>(null);
	const list = actions.data?.actions ?? [];

	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<Power className="size-4" />
					{m.host_power_title()}
				</CardTitle>
			</CardHeader>
			<CardContent className="space-y-3">
				<QueryState
					isLoading={actions.isLoading}
					error={actions.error}
					refetch={actions.refetch}
				>
					<div className="flex flex-wrap items-center gap-3">
						{list.map((a) => (
							<Button
								key={a.id}
								variant={a.danger ? "destructive" : "outline"}
								size="sm"
								disabled={!a.available}
								title={a.unavailable_reason ?? undefined}
								onClick={() => {
									setSent(null);
									setConfirming(a);
								}}
							>
								{actionTitle(a)}
							</Button>
						))}
					</div>
					{list
						.filter((a) => !a.available && a.unavailable_reason)
						.map((a) => (
							<p key={a.id} className="text-xs text-muted-foreground">
								{actionTitle(a)}: {a.unavailable_reason}
							</p>
						))}
					{sent && <p className="text-sm">{sent}</p>}
				</QueryState>
				{confirming && (
					<ConfirmDialog
						action={confirming}
						onClose={() => setConfirming(null)}
						onAccepted={(a) => {
							setConfirming(null);
							setSent(m.host_power_sent({ action: actionTitle(a) }));
						}}
					/>
				)}
			</CardContent>
		</Card>
	);
};

/** Runs a host action once the console password is re-entered; the BFF verifies it. */
export const ConfirmDialog: FC<{
	action: ActionInfo;
	onClose: () => void;
	onAccepted: (action: ActionInfo) => void;
}> = ({ action, onClose, onAccepted }) => {
	const [password, setPassword] = useState("");
	const refusal = usePasswordFailure();
	const [error, setError] = useState<string | null>(null);
	const [busy, setBusy] = useState(false);

	const submit = async () => {
		setBusy(true);
		setError(null);
		refusal.reset();
		try {
			await apiFetch(`/api/v1/actions/${encodeURIComponent(action.id)}`, {
				method: "POST",
				headers: { "Content-Type": "application/json" },
				body: JSON.stringify({ password }),
			});
			onAccepted(action);
		} catch (e) {
			if (refusal.classify(e)) return;
			setError(
				(e instanceof ApiError && apiErrorMessage(e)) || m.common_error(),
			);
		} finally {
			setBusy(false);
		}
	};

	return (
		<Dialog open onOpenChange={(o) => !o && onClose()}>
			<DialogContent>
				<DialogHeader>
					<DialogTitle>
						{m.host_power_confirm_title({ action: actionTitle(action) })}
					</DialogTitle>
					<DialogDescription>{m.host_power_confirm_body()}</DialogDescription>
				</DialogHeader>
				<form
					className="space-y-3"
					onSubmit={(e) => {
						e.preventDefault();
						void submit();
					}}
				>
					<PasswordConfirmField
						id="host-power-password"
						value={password}
						onChange={setPassword}
						failure={refusal.failure}
						autoFocus
					/>
					{error && <p className="text-sm text-destructive">{error}</p>}
					<DialogFooter>
						<Button
							type="submit"
							variant={action.danger ? "destructive" : "default"}
							disabled={busy || password.length === 0}
						>
							{busy ? m.host_power_working() : actionTitle(action)}
						</Button>
					</DialogFooter>
				</form>
			</DialogContent>
		</Dialog>
	);
};
