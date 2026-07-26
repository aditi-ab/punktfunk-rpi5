import { useQueryClient } from "@tanstack/react-query";
import type { FC } from "react";
import { getGetStatusQueryKey, useGetStatus } from "@/api/gen/host/host";
import { useGetLibrary } from "@/api/gen/library/library";
import type { ActiveGame } from "@/api/gen/model/activeGame";
import {
	useEndGame,
	useRequestIdr,
	useStopSession,
} from "@/api/gen/session/session";
import { useLocale } from "@/lib/i18n";
import { DashboardView } from "./view";

export const SectionDashboard: FC = () => {
	useLocale();
	const qc = useQueryClient();
	// Poll live status every 2s so the console tracks an active session.
	const status = useGetStatus({ query: { refetchInterval: 2_000 } });
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

	/**
	 * "End now" means two different things, and which one is right follows from the row's state: a
	 * game whose session is still live ends by stopping that session (what then happens to the game
	 * follows the operator's policy — stopping a session is not licence to close a game), while a
	 * game already waiting out its reconnect window has no session left to stop and is ended directly.
	 */
	const onEndGame = (game: ActiveGame) => {
		if (game.state === "grace") {
			endGame.mutate(
				{ data: { app_id: game.app_id ?? null } },
				{ onSuccess: invalidate },
			);
		} else {
			stop.mutate(undefined, { onSuccess: invalidate });
		}
	};

	return (
		<DashboardView
			status={status}
			library={library.data}
			onStopSession={() => stop.mutate(undefined, { onSuccess: invalidate })}
			onRequestIdr={() => idr.mutate(undefined)}
			onEndGame={onEndGame}
			isStopping={stop.isPending}
			isRequestingIdr={idr.isPending}
			isEndingGame={endGame.isPending || stop.isPending}
		/>
	);
};
