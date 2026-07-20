import { createFileRoute } from "@tanstack/react-router";
import { SectionStore } from "@/sections/Store";

// The plugin store. Sits at /plugins as an index route, so it coexists with the per-plugin UI route
// (`plugins.$pluginId.$.tsx`) without either shadowing the other.
export const Route = createFileRoute("/plugins/")({ component: SectionStore });
