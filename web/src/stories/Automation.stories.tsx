import type { Meta, StoryObj } from "@storybook/react-vite";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { getGetLibraryPageQueryKey } from "@/api/gen/library/library";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import type { NativeClient } from "@/api/gen/model/nativeClient";
import { getListNativeClientsQueryKey } from "@/api/gen/native/native";
import { HookForm } from "@/sections/Automation/HookForm";
import { nativeClients } from "./lib/fixtures";

/**
 * Adding an automation (`HookForm`).
 *
 * The two filter fields offer what the host knows and stay free text, because a hook may name
 * a game that is not installed yet. The game field asks the host for one page of matches, so
 * the story seeds the page an empty field asks for; typing needs a host.
 */
/** A stand-in cover, so the artwork path is visible without a host to serve real ones.
 *  Every fourth title ships none, which is what exercises the monogram fallback. */
const cover = (i: number) =>
	`data:image/svg+xml;utf8,${encodeURIComponent(
		`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 60"><rect width="40" height="60" fill="hsl(${(i * 47) % 360} 45% 38%)"/><circle cx="20" cy="24" r="9" fill="hsl(${(i * 47) % 360} 60% 70%)"/></svg>`,
	)}`;

const game = (i: number): GameEntry => ({
	art: i % 4 === 3 ? {} : { portrait: cover(i) },
	id: `steam:${100000 + i}`,
	store: "steam",
	title: `${TITLES[i % TITLES.length]} ${Math.floor(i / TITLES.length) + 1}`,
});

const TITLES = [
	"Hades",
	"Celeste",
	"Hollow Knight",
	"Dota",
	"Factorio",
	"Stardew Valley",
	"Portal",
	"Terraria",
];

/** What the form asks for: `HookForm`'s `SUGGESTIONS`. */
const PAGE = 50;

/** Seed the cache the form reads, so the story needs no host. */
function Seeded({
	total,
	children,
}: {
	total: number;
	children: React.ReactNode;
}) {
	const qc = useQueryClient();
	useState(() => {
		const key = getGetLibraryPageQueryKey({ q: "", limit: PAGE });
		qc.setQueryDefaults(key, { staleTime: Infinity });
		qc.setQueryData(key, {
			items: Array.from({ length: Math.min(total, PAGE) }, (_, i) => ({
				...game(i),
				hidden: false,
			})),
			total,
			platforms: [],
			...(total > PAGE ? { next_cursor: "next" } : {}),
		});
		qc.setQueryData(
			getListNativeClientsQueryKey(),
			nativeClients as NativeClient[],
		);
		return null;
	});
	return <>{children}</>;
}

function Harness({ total }: { total: number }) {
	const [value, setValue] = useState<{
		on: string;
		run?: string | null;
		filter?: { client?: string | null; app?: string | null };
	} | null>({ on: "stream.started", run: "", filter: { app: "" } });
	return (
		<Seeded total={total}>
			<HookForm
				value={value}
				onCancel={() => setValue(null)}
				onSave={() => setValue(null)}
			/>
		</Seeded>
	);
}

const meta = {
	title: "Console/Automation",
	parameters: { layout: "padded" },
} satisfies Meta;
export default meta;

type Story = StoryObj<typeof meta>;

/** A handful of games, the ordinary case. */
export const AddHook: Story = { render: () => <Harness total={12} /> };

/** Ten thousand of them: the form still holds one page. */
export const HugeLibrary: Story = { render: () => <Harness total={10_000} /> };
