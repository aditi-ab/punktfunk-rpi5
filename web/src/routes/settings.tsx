import { createFileRoute } from "@tanstack/react-router";
import { SectionSettings } from "@/sections/Settings";

export const Route = createFileRoute("/settings")({
	component: SectionSettings,
});
