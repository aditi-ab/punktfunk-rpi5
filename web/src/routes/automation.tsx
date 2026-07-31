import { createFileRoute } from "@tanstack/react-router";
import { SectionAutomation } from "@/sections/Automation";

export const Route = createFileRoute("/automation")({
	component: SectionAutomation,
});
