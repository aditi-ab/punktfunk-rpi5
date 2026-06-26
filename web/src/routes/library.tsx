import { createFileRoute } from "@tanstack/react-router";
import { SectionLibrary } from "@/sections/Library";

export const Route = createFileRoute("/library")({ component: SectionLibrary });
