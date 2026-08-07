import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC } from "react";
import { getGetStatusQueryKey, useGetStatus } from "@/api/gen/host/host";
import { useGetLibrary } from "@/api/gen/library/library";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import {
	useEndGame,
	useRequestIdr,
	useStopSession,
} from "@/api/gen/session/session";
import { useDialogs } from "@/components/dialogs";
import { apiErrorMessage } from "@/lib/errors";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { DashboardView } from "./view";

export const SectionDashboard: FC = () => {
	useLocale();
	const qc = useQueryClient();
	const { confirm } = useDialogs();
	// Session/game transitions arrive on the event stream now (api/events.ts invalidates this key),
	// so the timer only has to cover what events cannot: the live stream numbers — codec, resolution,
	// fps, bitrate — which change continuously while something is streaming. Idle, it is a slow
	// safety net in case the stream is unavailable.
	const status = useGetStatus({
		query: {
			refetchInterval: (q) =>
				q.state.data?.video_streaming || (q.state.data?.games?.length ?? 0) > 0
					? 2_000
					: 15_000,
		},
	});
	// The catalog, for the running-game card's box art. Fetched once and held: a library scan touches
	// every installed store's on-disk metadata, so it must not ride the 2 s status poll.
	const library = useGetLibrary(undefined, {
		query: { staleTime: 5 * 60_000 },
	});
	const stop = useStopSession();
	const idr = useRequestIdr();
	const endGame = useEndGame();

	const invalidate = () =>
		qc.invalidateQueries({ queryKey: getGetStatusQueryKey() });

	/** Every session control reports its failure. These are the console's most consequential
	 * buttons — stopping a session, ending a game — and a refusal used to be completely silent. */
	const failed = (fallback: string) => (e: unknown) =>
		toast.error(apiErrorMessage(e) ?? fallback);

	/**
	 * "End now" means two different things, and which one is right follows from the row's state: a
	 * game whose session is still live ends by stopping that session (what then happens to the game
	 * follows the operator's policy — stopping a session is not licence to close a game), while a
	 * game already waiting out its reconnect window has no session left to stop and is ended directly.
	 *
	 * Both paths are wider than the row they are attached to, and neither used to say so:
	 *
	 * - `DELETE /session` is the host's ONLY stop and it tears down every live session
	 *   (mgmt/session.rs calls `quit_session` AND `session_status::stop_all_quit`). With two people
	 *   streaming, "End now" on one row kicked both. There is no per-session stop to call instead,
	 *   so the honest fix is to name the blast radius before doing it.
	 * - `POST /game/end` with `app_id: null` means "end EVERY waiting game" to the host, and a grace
	 *   row for an operator-typed command carries no `app_id` — so that row ended all of them.
	 */
	const onEndGame = async (game: ActiveGame) => {
		const games = status.data?.games ?? [];
		if (game.state === "grace") {
			const waiting = games.filter((g) => g.state === "grace").length;
			if (!game.app_id && waiting > 1) {
				const ok = await confirm({
					title: m.games_end_all_waiting_title({ count: waiting }),
					description: m.games_end_all_waiting_confirm({ count: waiting }),
					confirmLabel: m.games_end_now(),
					destructive: true,
				});
				if (!ok) return;
			}
			endGame.mutate(
				{ data: { app_id: game.app_id ?? null } },
				{ onSuccess: invalidate, onError: failed(m.games_end_failed()) },
			);
			return;
		}
		if (!(await confirmStopAll())) return;
		stop.mutate(undefined, {
			onSuccess: invalidate,
			onError: failed(m.action_stop_failed()),
		});
	};

	/** Shared by "End now" on a live row and the card's own Stop-session button: with more than one
	 * session live, stopping is not a per-client action and the operator has to know that. */
	const confirmStopAll = (): Promise<boolean> => {
		const active = status.data?.active_sessions ?? 0;
		if (active <= 1) return Promise.resolve(true);
		return confirm({
			title: m.action_stop_session_all_title(),
			description: m.action_stop_session_all_confirm({ count: active }),
			confirmLabel: m.action_stop_session_all(),
			destructive: true,
		});
	};

	return (
		<DashboardView
			status={status}
			library={library.data}
			onStopSession={async () => {
				if (!(await confirmStopAll())) return;
				stop.mutate(undefined, {
					onSuccess: invalidate,
					onError: failed(m.action_stop_failed()),
				});
			}}
			onRequestIdr={() =>
				idr.mutate(undefined, { onError: failed(m.action_idr_failed()) })
			}
			onEndGame={onEndGame}
			isStopping={stop.isPending}
			isRequestingIdr={idr.isPending}
			isEndingGame={endGame.isPending || stop.isPending}
		/>
	);
};
