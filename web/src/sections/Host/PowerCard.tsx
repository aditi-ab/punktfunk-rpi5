import { type FC, useState } from "react";
import { useListActions } from "@/api/gen/actions/actions";
import type { ActionInfo } from "@/api/gen/model";
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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { m } from "@/paraglide/messages";

/** Localized titles for the KNOWN action ids; unknown ids fall back to the server's title —
 * the contract that lets future host actions appear with no console release. */
const actionTitle = (a: ActionInfo): string => {
	switch (a.id) {
		case "power.sleep":
			return m.host_power_sleep();
		case "power.reboot":
			return m.host_power_reboot();
		case "power.shutdown":
			return m.host_power_shutdown();
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
				<CardTitle>{m.host_power_title()}</CardTitle>
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

/** The password-confirm dialog — the update-apply recipe: plain fetch (a 401 here is a wrong
 * password, not an expired session), password verified and stripped in the BFF. */
const ConfirmDialog: FC<{
	action: ActionInfo;
	onClose: () => void;
	onAccepted: (action: ActionInfo) => void;
}> = ({ action, onClose, onAccepted }) => {
	const [password, setPassword] = useState("");
	const [error, setError] = useState<string | null>(null);
	const [busy, setBusy] = useState(false);

	const submit = async () => {
		setBusy(true);
		setError(null);
		try {
			const res = await fetch(
				`/api/v1/actions/${encodeURIComponent(action.id)}`,
				{
					method: "POST",
					headers: { "content-type": "application/json" },
					credentials: "same-origin",
					body: JSON.stringify({ password }),
				},
			);
			if (res.status === 202) {
				onAccepted(action);
				return;
			}
			const body = (await res.json().catch(() => null)) as {
				error?: string;
			} | null;
			if (res.status === 401) setError(m.update_apply_wrong_password());
			else if (res.status === 429) setError(m.update_apply_throttled());
			else setError(body?.error ?? `HTTP ${res.status}`);
		} catch {
			setError(m.common_error());
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
					<div className="space-y-1.5">
						<Label htmlFor="host-power-password">
							{m.update_apply_password_label()}
						</Label>
						<Input
							id="host-power-password"
							type="password"
							autoFocus
							value={password}
							onChange={(e) => setPassword(e.target.value)}
							autoComplete="current-password"
						/>
					</div>
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
