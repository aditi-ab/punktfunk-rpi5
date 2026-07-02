import type { FC } from "react";
import { useGetHostInfo, useListCompositors } from "@/api/gen/host/host";
import { useLocale } from "@/lib/i18n";
import { GpuSection } from "./GpuCard";
import { HostView } from "./view";

export const SectionHost: FC = () => {
	useLocale();
	const host = useGetHostInfo();
	const compositors = useListCompositors();

	return (
		<HostView host={host} compositors={compositors} gpu={<GpuSection />} />
	);
};
