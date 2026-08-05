import type { Meta, StoryObj } from "@storybook/react-vite";
import { GameForm } from "@/sections/Library/GameForm";
import { LibraryGrid } from "@/sections/Library/LibraryGrid";
import { MigrationBanner, SourcesCard } from "@/sections/Library/Sources";
import { library } from "./lib/fixtures";

const noop = () => {};
const idle = { isLoading: false, error: null, refetch: noop };
const emptyForm = {
	title: "",
	portrait: "",
	hero: "",
	header: "",
	logo: "",
	command: "",
	platform: "",
	description: "",
	developer: "",
	publisher: "",
	releaseYear: "",
	genres: "",
	tags: "",
	region: "",
	players: "",
};

// The overview grid and the add/edit form are separate components now, so the stories
// render each on its own (no combined page view).
const meta = {
	title: "Pages/Library",
	parameters: { layout: "padded" },
} satisfies Meta;

export default meta;
type Story = StoryObj;

export const Populated: Story = {
	render: () => (
		<LibraryGrid
			library={{ data: library, ...idle }}
			onEdit={noop}
			onDelete={noop}
			deletingId={null}
		/>
	),
};

/** Launcher entries (design D4) get their own rail above the grid. */
export const WithLaunchers: Story = {
	render: () => (
		<LibraryGrid
			library={{
				data: [
					{
						id: "steam:bigpicture",
						store: "steam",
						title: "Steam Big Picture",
						art: { portrait: null, hero: null, logo: null, header: null },
						role: "launcher",
						launch: { kind: "steam_ui", value: "bigpicture" },
					},
					...library,
				],
				...idle,
			}}
			onEdit={noop}
			onDelete={noop}
			deletingId={null}
		/>
	),
};

export const Empty: Story = {
	render: () => (
		<LibraryGrid
			library={{ data: [], ...idle }}
			onEdit={noop}
			onDelete={noop}
			deletingId={null}
		/>
	),
};

/** A catalog row for the "Add a source" rail — only the fields the card actually reads. */
const catalogEntry = (
	over: Partial<Parameters<typeof SourcesCard>[0]["available"][number]>,
) =>
	({
		id: "steam",
		pkg: "@punktfunk/plugin-steam",
		title: "Steam",
		description: "Steam library scanner",
		author: "unom",
		version: "0.1.0",
		source: "unom",
		tier: "verified",
		platforms: [],
		compatible: true,
		update_available: false,
		categories: ["library"],
		...over,
	}) as Parameters<typeof SourcesCard>[0]["available"][number];

const sourcesArgs = {
	available: [],
	running: new Set<string>(),
	busyId: null,
	installBusy: false,
	activeFilter: null,
	onToggle: noop,
	onFilter: noop,
	onSettings: noop,
	onPurge: noop,
	onInstall: noop,
};

/** The bridge-release shape: built-in scanners only, one turned off. */
export const Sources: Story = {
	render: () => (
		<SourcesCard
			{...sourcesArgs}
			sources={[
				{ id: "steam", label: "Steam", enabled: true, origin: "builtin" },
				{ id: "lutris", label: "Lutris", enabled: false, origin: "builtin" },
				{
					id: "heroic",
					label: "Heroic (Epic / GOG / Amazon)",
					enabled: true,
					origin: "builtin",
				},
			]}
		/>
	),
};

/** Mid-migration: a claimed plugin source beside the remaining built-ins, one plugin stopped. */
export const SourcesWithPlugins: Story = {
	render: () => (
		<SourcesCard
			{...sourcesArgs}
			sources={[
				{
					id: "steam",
					label: "Steam",
					enabled: true,
					origin: "plugin",
					provider: "steam",
					entries: 214,
				},
				{
					id: "lutris",
					label: "Lutris",
					enabled: false,
					origin: "plugin",
					provider: "lutris",
					entries: 12,
				},
				{
					id: "heroic",
					label: "Heroic (Epic / GOG / Amazon)",
					enabled: true,
					origin: "builtin",
				},
			]}
			running={new Set(["steam"])}
			available={[
				catalogEntry({ pkg: "@punktfunk/plugin-heroic", title: "Heroic" }),
			]}
		/>
	),
};

/** A fresh host after extraction: nothing installed, two launchers detected on this box. */
export const SourcesEmptyWithDetected: Story = {
	render: () => (
		<SourcesCard
			{...sourcesArgs}
			sources={[]}
			available={[
				catalogEntry({ detected: true }),
				catalogEntry({
					pkg: "@punktfunk/plugin-lutris",
					title: "Lutris",
					detected: true,
				}),
				catalogEntry({
					pkg: "@punktfunk/plugin-heroic",
					title: "Heroic",
					detected: false,
				}),
			]}
		/>
	),
};

/** The bridge-release nudge — one button per still-built-in scanner, never an auto-install. */
export const Migration: Story = {
	render: () => (
		<MigrationBanner
			rows={[
				{
					source: {
						id: "steam",
						label: "Steam",
						enabled: true,
						origin: "builtin",
					},
					entry: catalogEntry({}),
				},
				{
					source: {
						id: "lutris",
						label: "Lutris",
						enabled: true,
						origin: "builtin",
					},
					entry: catalogEntry({
						pkg: "@punktfunk/plugin-lutris",
						id: "lutris",
						title: "Lutris",
					}),
				},
			]}
			busy={false}
			onInstall={noop}
		/>
	),
};

export const AddForm: Story = {
	render: () => (
		<GameForm
			initial={emptyForm}
			mode="add"
			onSubmit={noop}
			onCancel={noop}
			isSaving={false}
		/>
	),
};
