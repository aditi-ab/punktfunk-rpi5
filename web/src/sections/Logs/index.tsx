import type { FC } from "react";
import { useLocale } from "@/lib/i18n";
import { LogsSection } from "./LogsCard";
import { LogsView } from "./view";

// Logs = one self-contained viewer card owning its polling; this container only binds the layout.
export const SectionLogs: FC = () => {
	useLocale();
	return <LogsView viewer={<LogsSection />} />;
};
