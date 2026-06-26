import { useQueryClient } from "@tanstack/react-query";
import type { FC } from "react";
import { getGetStatusQueryKey, useGetStatus } from "@/api/gen/host/host";
import { useRequestIdr, useStopSession } from "@/api/gen/session/session";
import { useLocale } from "@/lib/i18n";
import { DashboardView } from "./view";

export const SectionDashboard: FC = () => {
	useLocale();
	const qc = useQueryClient();
	// Poll live status every 2s so the console tracks an active session.
	const status = useGetStatus({ query: { refetchInterval: 2_000 } });
	const stop = useStopSession();
	const idr = useRequestIdr();

	const invalidate = () =>
		qc.invalidateQueries({ queryKey: getGetStatusQueryKey() });

	return (
		<DashboardView
			status={status}
			onStopSession={() => stop.mutate(undefined, { onSuccess: invalidate })}
			onRequestIdr={() => idr.mutate(undefined)}
			isStopping={stop.isPending}
			isRequestingIdr={idr.isPending}
		/>
	);
};
