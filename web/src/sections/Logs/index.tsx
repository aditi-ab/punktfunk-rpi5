import type { FC } from "react";
import { useLocale } from "@/lib/i18n";
import { ClientLogsSection } from "./ClientLogsCard";
import { LogsSection } from "./LogsCard";
import { LogsView } from "./view";

// Logs = one self-contained viewer card owning its polling; this container only binds the layout.
// Client-uploaded bundles ("Send logs to host") render beneath the live host log — same page a
// reporter already exports the host log from, so both halves of a report live in one place.
export const SectionLogs: FC = () => {
	useLocale();
	return (
		<LogsView
			viewer={
				<>
					<LogsSection />
					<ClientLogsSection />
				</>
			}
		/>
	);
};
