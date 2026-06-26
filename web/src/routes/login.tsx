import { createFileRoute } from "@tanstack/react-router";
import { SectionLogin } from "@/sections/Login";

export const Route = createFileRoute("/login")({
	validateSearch: (s: Record<string, unknown>): { next?: string } => ({
		next: typeof s.next === "string" ? s.next : undefined,
	}),
	component: RouteComponent,
});

function RouteComponent() {
	const { next } = Route.useSearch();
	return <SectionLogin next={next} />;
}
