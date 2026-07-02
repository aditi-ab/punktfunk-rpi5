import { createFileRoute } from "@tanstack/react-router";
import { SectionLogs } from "@/sections/Logs";

export const Route = createFileRoute("/logs")({ component: SectionLogs });
