// Container for Host's settings: the settings query, one-setting writes, the restart flow, and
// the app list the voice-chat picker suggests from. Everything visual is in `view.tsx`.
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, type ReactNode, useEffect, useState } from "react";
import { useListActions } from "@/api/gen/actions/actions";
import {
	getGetHostSettingsQueryKey,
	patchHostSettings,
	useGetHostSettings,
	useGetPlayingApps,
} from "@/api/gen/host/host";
import { PasswordConfirmDialog } from "@/components/password-confirm";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { touchesPlane } from "@/lib/password-gates";
import { m } from "@/paraglide/messages";
import { GpuRow } from "../Host/GpuRow";
import { ConfirmDialog } from "../Host/PowerCard";
import { labelOf } from "./controls";
import { RestartBanner } from "./RestartBanner";
import { SessionGameRows } from "./SessionGameRows";
import { HostSettingsView } from "./view";

/** How long a restart may take before the page stops waiting and says so. */
const RESTART_PATIENCE_MS = 90_000;

export const HostSettings: FC<{ top?: ReactNode }> = ({ top }) => {
	useLocale();
	const qc = useQueryClient();
	// Set when a restart is accepted; the page polls until the new process answers.
	const [restartingSince, setRestartingSince] = useState<number | null>(null);
	const restarting = restartingSince != null;
	const state = useGetHostSettings({
		query: { refetchInterval: restarting ? 2_000 : false },
	});
	const actions = useListActions();
	const restartAction = actions.data?.actions.find(
		(a) => a.id === "host.restart",
	);
	const [confirming, setConfirming] = useState(false);
	const hasVoiceApps = !!state.data?.settings.some(
		(s) => s.id === "audio_voice_apps",
	);
	const apps = useGetPlayingApps({
		query: { enabled: hasVoiceApps, refetchInterval: 15_000 },
	});
	const [pending, setPending] = useState<ReadonlySet<string>>(new Set());

	// The old process reports the pending list until it exits, so an empty list fetched after
	// the restart was accepted can only come from the new one.
	useEffect(() => {
		if (restartingSince == null) return;
		if (
			state.data?.restart_pending.length === 0 &&
			state.dataUpdatedAt > restartingSince
		) {
			setRestartingSince(null);
			toast.success(m.host_settings_restarted());
		}
	}, [restartingSince, state.data, state.dataUpdatedAt]);

	useEffect(() => {
		if (restartingSince == null) return;
		const giveUp = setTimeout(() => {
			setRestartingSince(null);
			toast.error(m.host_settings_restart_slow());
		}, RESTART_PATIENCE_MS);
		return () => clearTimeout(giveUp);
	}, [restartingSince]);

	const settle = (id: string, busy: boolean) =>
		setPending((prev) => {
			const next = new Set(prev);
			if (busy) next.add(id);
			else next.delete(id);
			return next;
		});

	// A plane setting waits for the console password; until then the control shows the stored value.
	const [asking, setAsking] = useState<{ id: string; value: unknown } | null>(
		null,
	);
	const askingRow = state.data?.settings.find((s) => s.id === asking?.id);

	// One key per request: the answer is the whole new state, so it replaces the cache outright.
	const save = async (id: string, value: unknown, password?: string) => {
		// The console's server reads the password from the body and strips it.
		const next = await patchHostSettings({ [id]: value, password });
		qc.setQueryData(getGetHostSettingsQueryKey(), next);
	};
	const onSet = async (id: string, value: unknown) => {
		if (touchesPlane({ [id]: value })) {
			setAsking({ id, value });
			return;
		}
		settle(id, true);
		try {
			await save(id, value);
		} catch (e) {
			toast.error(apiErrorMessage(e) ?? m.host_settings_save_failed());
			qc.invalidateQueries({ queryKey: getGetHostSettingsQueryKey() });
		} finally {
			settle(id, false);
		}
	};

	const waiting = (state.data?.settings ?? [])
		.filter((s) => s.restart_pending)
		.map(labelOf);

	return (
		<>
			<HostSettingsView
				// A host that is restarting fails its polls; that is the wait, not an error.
				state={{ ...state, error: restarting ? null : state.error }}
				pending={pending}
				onSet={onSet}
				playingApps={apps.data?.apps}
				extra={{ video: <GpuRow />, session: <SessionGameRows /> }}
				top={top}
				banner={
					<RestartBanner
						names={waiting}
						action={restartAction}
						restarting={restarting}
						onRestart={() => setConfirming(true)}
					/>
				}
			/>
			<PasswordConfirmDialog
				open={asking !== null}
				id="host-settings-password"
				title={m.host_settings_confirm_title({
					name: askingRow ? labelOf(askingRow) : "",
				})}
				body={m.host_settings_confirm_body()}
				submitLabel={m.common_save()}
				failedText={m.host_settings_save_failed()}
				onSubmit={async (password) => {
					if (asking) await save(asking.id, asking.value, password);
				}}
				onClose={() => setAsking(null)}
			/>
			{confirming && restartAction && (
				<ConfirmDialog
					action={restartAction}
					onClose={() => setConfirming(false)}
					onAccepted={() => {
						setConfirming(false);
						setRestartingSince(Date.now());
					}}
				/>
			)}
		</>
	);
};
