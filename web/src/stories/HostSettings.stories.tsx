import type { Meta, StoryObj } from "@storybook/react-vite";
import { useState } from "react";
import type { HostSettingsState, SettingState } from "@/api/gen/model";
import { labelOf } from "@/sections/HostSettings/controls";
import { RestartBanner } from "@/sections/HostSettings/RestartBanner";
import { HostSettingsView } from "@/sections/HostSettings/view";
import { Routed } from "./lib/routed";

/**
 * Host → Settings (design/host-settings-console.md §5).
 *
 * The fixture covers every state a row can be in: at its default, changed in the console, pinned
 * by host.env, pinned by a command-line flag, waiting for a restart, and advanced (hidden until the
 * switch is on). Writes apply locally so the controls can be clicked.
 */
const row = (
	r: Partial<SettingState> & Pick<SettingState, "id">,
): SettingState => ({
	group: "streaming",
	kind: "bool",
	default: false,
	value: false,
	stored: null,
	source: "default",
	origin: null,
	env: `PUNKTFUNK_${r.id.toUpperCase()}`,
	apply: "next_session",
	advanced: false,
	title: r.id,
	docs: "configuration",
	restart_pending: false,
	...r,
});

const settings: HostSettingsState = {
	env_file: "/home/me/.config/punktfunk/host.env",
	restart_pending: ["gamestream"],
	settings: [
		row({
			id: "gamestream",
			value: true,
			stored: true,
			source: "store",
			apply: "restart",
			restart_pending: true,
		}),
		row({ id: "webtransport", apply: "restart" }),
		row({
			id: "clipboard",
			kind: "enum",
			options: ["off", "text", "files"],
			default: "off",
			value: "text",
			stored: "text",
			source: "store",
		}),
		row({
			id: "host_name",
			kind: "text",
			max_len: 63,
			default: "",
			value: "Living Room",
			source: "flag",
			origin: "--host-name",
			apply: "restart",
		}),
		row({ id: "ten_bit", group: "video", default: true, value: true }),
		row({
			id: "chroma_444",
			group: "video",
			default: true,
			value: false,
			source: "env",
			origin: "PUNKTFUNK_444",
			stored: true,
		}),
		row({
			id: "max_fps",
			group: "video",
			kind: "int",
			min: 0,
			max: 240,
			unit: "fps",
			default: 0,
			value: 60,
			stored: 60,
			source: "store",
		}),
		row({
			id: "audio_output_mode",
			group: "audio",
			kind: "enum",
			options: ["client_only", "host_and_client", "follow_default"],
			default: "client_only",
			value: "client_only",
		}),
		row({
			id: "audio_voice_chat",
			group: "audio",
			kind: "enum",
			options: ["stream", "host"],
			default: "stream",
			value: "host",
			stored: "host",
			source: "store",
		}),
		row({
			id: "audio_voice_apps",
			group: "audio",
			kind: "list",
			default: ["discord", "mumble"],
			value: ["discord", "mumble"],
			advanced: true,
		}),
	],
};

function Live({ initial }: { initial: HostSettingsState }) {
	const [state, setState] = useState(initial);
	const onSet = (id: string, value: unknown) =>
		setState((s) => ({
			...s,
			settings: s.settings.map((r) =>
				r.id === id
					? {
							...r,
							value: value ?? r.default,
							stored: value,
							source: value == null ? "default" : "store",
						}
					: r,
			),
		}));
	return (
		<HostSettingsView
			state={{ data: state, isLoading: false, error: null }}
			pending={new Set()}
			onSet={onSet}
			playingApps={["discord", "firefox", "spotify"]}
			banner={
				<RestartBanner
					names={state.settings.filter((r) => r.restart_pending).map(labelOf)}
					action={{
						id: "host.restart",
						title: "Restart Punktfunk",
						group: "host",
						danger: true,
						available: true,
						permitted: true,
					}}
					restarting={false}
					onRestart={() => {}}
				/>
			}
		/>
	);
}

const meta = {
	title: "Pages/Host settings",
	parameters: { layout: "padded" },
} satisfies Meta;
export default meta;

type Story = StoryObj<typeof meta>;

export const Default: Story = {
	render: () => (
		<Routed>
			<Live initial={settings} />
		</Routed>
	),
};

export const Loading: Story = {
	render: () => (
		<Routed>
			<HostSettingsView
				state={{ data: undefined, isLoading: true, error: null }}
				pending={new Set()}
				onSet={() => {}}
			/>
		</Routed>
	),
};
