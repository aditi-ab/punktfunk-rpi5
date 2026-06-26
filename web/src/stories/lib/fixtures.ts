// Mock API payloads for the page stories — typed against the generated models so
// they stay honest if the OpenAPI schema changes.
import type { AvailableCompositor } from "@/api/gen/model/availableCompositor";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import type { PairedClient } from "@/api/gen/model/pairedClient";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";

export const hostInfo: HostInfo = {
	abi_version: 2,
	app_version: "7.1.450.0",
	codecs: ["h264", "h265", "av1"],
	gfe_version: "3.23.0.74",
	hostname: "ENRICOS-DESKTOP",
	local_ip: "192.168.1.173",
	ports: {
		audio: 48000,
		control: 47999,
		http: 47989,
		https: 47984,
		mgmt: 47990,
		rtsp: 48010,
		video: 47998,
	},
	uniqueid: "0f8a1c3e9b7d4a62",
	version: "0.2.0",
};

export const compositors: AvailableCompositor[] = [
	{ id: "kwin", label: "KWin (Plasma)", available: true, default: true },
	{ id: "gamescope", label: "gamescope", available: true, default: false },
	{ id: "mutter", label: "Mutter (GNOME)", available: false, default: false },
	{ id: "wlroots", label: "Sway / wlroots", available: false, default: false },
];

export const statusActive: RuntimeStatus = {
	video_streaming: true,
	audio_streaming: true,
	paired_clients: 3,
	pin_pending: false,
	session: { width: 5120, height: 1440, fps: 240 },
	stream: {
		codec: "h265",
		width: 5120,
		height: 1440,
		fps: 240,
		bitrate_kbps: 150_000,
		min_fec: 5,
		packet_size: 1392,
	},
};

export const statusIdle: RuntimeStatus = {
	video_streaming: false,
	audio_streaming: false,
	paired_clients: 1,
	pin_pending: true,
	session: null,
	stream: null,
};

export const pairedClients: PairedClient[] = [
	{
		fingerprint:
			"a1b2c3d4e5f60718293a4b5c6d7e8f90112233445566778899aabbccddeeff00",
		subject: "enricos-macbook",
		not_before_unix: 1_718_000_000,
		not_after_unix: 2_030_000_000,
	},
	{
		fingerprint:
			"ff00eeddccbbaa998877665544332211009f8e7d6c5b4a39281706f5e4d3c2b1",
		subject: "living-room-tv",
		not_before_unix: 1_718_500_000,
		not_after_unix: 2_030_000_000,
	},
	{
		fingerprint:
			"0011223344556677889900aabbccddeeff112233445566778899aabbccddeeff",
		subject: null,
	},
];

const noArt = { header: null, hero: null, logo: null, portrait: null };
export const library: GameEntry[] = [
	{
		id: "steam:1245620",
		store: "steam",
		title: "Elden Ring",
		art: noArt,
		launch: null,
	},
	{
		id: "steam:1086940",
		store: "steam",
		title: "Baldur's Gate 3",
		art: noArt,
		launch: null,
	},
	{
		id: "steam:413150",
		store: "steam",
		title: "Stardew Valley",
		art: noArt,
		launch: null,
	},
	{
		id: "custom:retroarch",
		store: "custom",
		title: "RetroArch",
		art: noArt,
		launch: null,
	},
];
