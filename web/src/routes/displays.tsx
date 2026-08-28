import { createFileRoute } from "@tanstack/react-router";
import { SectionDisplays } from "@/sections/Displays";

export const Route = createFileRoute("/displays")({
	component: SectionDisplays,
});
