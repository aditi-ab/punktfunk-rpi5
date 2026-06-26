import { createFileRoute } from "@tanstack/react-router";
import { SectionDashboard } from "@/sections/Dashboard";

export const Route = createFileRoute("/")({ component: SectionDashboard });
