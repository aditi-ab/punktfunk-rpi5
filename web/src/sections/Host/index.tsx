import type { FC } from "react";
import { useGetHostInfo, useListCompositors } from "@/api/gen/host/host";
import { useLocale } from "@/lib/i18n";
import { ConflictsCard } from "./ConflictsCard";
import { GpuSection } from "./GpuCard";
import { UpdateSection } from "./UpdateCard";
import { HostView } from "./view";

export const SectionHost: FC = () => {
	useLocale();
	const host = useGetHostInfo();
	const compositors = useListCompositors();

	return (
		<HostView
			host={host}
			compositors={compositors}
			conflicts={<ConflictsCard />}
			gpu={<GpuSection />}
			update={<UpdateSection />}
		/>
	);
};
