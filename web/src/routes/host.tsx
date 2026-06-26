import { createFileRoute } from "@tanstack/react-router";
import { SectionHost } from "@/sections/Host";

export const Route = createFileRoute("/host")({ component: SectionHost });
