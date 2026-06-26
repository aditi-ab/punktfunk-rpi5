import { createFileRoute } from "@tanstack/react-router";
import { SectionPairing } from "@/sections/Pairing";

export const Route = createFileRoute("/pairing")({ component: SectionPairing });
