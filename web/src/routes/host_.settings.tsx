import { createFileRoute } from "@tanstack/react-router";
import { SectionHostSettings } from "@/sections/HostSettings";

// `host_` keeps the page flat beside `/host` rather than nested inside its layout.
export const Route = createFileRoute("/host_/settings")({
	component: SectionHostSettings,
});
