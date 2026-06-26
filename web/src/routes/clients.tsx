import { createFileRoute } from "@tanstack/react-router";
import { SectionClients } from "@/sections/Clients";

export const Route = createFileRoute("/clients")({ component: SectionClients });
