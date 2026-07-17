import { createFileRoute } from "@tanstack/react-router";
import { SectionPlugin } from "@/sections/Plugins";

// A plugin's console-hosted UI (plugin-ui-surface). The `$` splat carries the plugin's own path so
// deep links survive a reload; the section maps it to the iframe src and keeps it in sync.
export const Route = createFileRoute("/plugins/$pluginId/$")({
	component: SectionPlugin,
});
