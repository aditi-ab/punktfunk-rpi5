import { createFileRoute } from "@tanstack/react-router";
import { SectionStats } from "@/sections/Stats";

export const Route = createFileRoute("/stats")({ component: SectionStats });
