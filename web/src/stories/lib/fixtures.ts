// Mock API payloads for the page stories — typed against the generated models so
// they stay honest if the OpenAPI schema changes.
import type { AvailableCompositor } from "@/api/gen/model/availableCompositor";
import type { Capture } from "@/api/gen/model/capture";
import type { CaptureMeta } from "@/api/gen/model/captureMeta";
import type { CustomPreset } from "@/api/gen/model/customPreset";
import type { DisplayPolicy } from "@/api/gen/model/displayPolicy";
import type { EffectivePolicy } from "@/api/gen/model/effectivePolicy";
import type { GameEntry } from "@/api/gen/model/gameEntry";
import type { HostInfo } from "@/api/gen/model/hostInfo";
import type { NativeClient } from "@/api/gen/model/nativeClient";
import type { NativePairStatus } from "@/api/gen/model/nativePairStatus";
import type { PairedClient } from "@/api/gen/model/pairedClient";
import type { PairingStatus } from "@/api/gen/model/pairingStatus";
import type { PendingDevice } from "@/api/gen/model/pendingDevice";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";
import type { StatsSample } from "@/api/gen/model/statsSample";
import type { StatsStatus } from "@/api/gen/model/statsStatus";

export const hostInfo: HostInfo = {
	abi_version: 2,
	app_version: "7.1.450.0",
	codecs: ["h264", "hevc", "av1"],
	gamestream: true,
	gfe_version: "3.23.0.74",
	hostname: "ENRICOS-DESKTOP",
	local_ip: "192.168.1.173",
	os: "linux/fedora/bazzite",
	os_name: "Bazzite 42 (Kinoite)",
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
	native_paired_clients: 2,
	pin_pending: false,
	active_sessions: 1,
	session: { width: 5120, height: 1440, fps: 240 },
	stream: {
		codec: "hevc",
		width: 5120,
		height: 1440,
		fps: 240,
		bitrate_kbps: 150_000,
		min_fec: 5,
		packet_size: 1392,
	},
	games: [
		{
			session_id: 1,
			client: "Living room TV",
			app_id: "steam:1145360",
			title: "Hades",
			store: "steam",
			plane: "native",
			state: "running",
		},
	],
};

export const statusIdle: RuntimeStatus = {
	video_streaming: false,
	audio_streaming: false,
	paired_clients: 1,
	native_paired_clients: 0,
	pin_pending: true,
	active_sessions: 0,
	session: null,
	stream: null,
	games: [],
};

/**
 * Idle, but a game its client walked away from is still running — on a countdown to being closed.
 * The state the running-game card exists for.
 */
export const statusGrace: RuntimeStatus = {
	...statusIdle,
	games: [
		{
			client: "Living room TV",
			app_id: "steam:1145360",
			title: "Hades",
			store: "steam",
			plane: "native",
			state: "grace",
			grace_remaining_s: 252,
		},
	],
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
		// Named by the operator — the row that shows what a rename buys you next to a sibling that
		// still reads as its (identical-for-everyone) certificate subject.
		label: "Living Room TV",
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
	// An emulated title with the metadata fields filled — exercises the platform
	// badge (non-PC) and the year in the caption.
	{
		id: "custom:sotc",
		store: "custom",
		title: "Shadow of the Colossus",
		art: noArt,
		launch: null,
		platform: "PS2",
		developer: "Team Ico",
		publisher: "Sony Computer Entertainment",
		release_year: 2005,
		genres: ["Adventure"],
		tags: ["favorite"],
		region: "PAL",
		players: 1,
	},
];

// --- Performance (stats) page ------------------------------------------------

export const statsStatusIdle: StatsStatus = {
	armed: false,
	kind: "native",
	sample_count: 0,
	started_unix_ms: 0,
	elapsed_ms: 0,
};

// A native-path pipeline: capture → submit → encode → send. Deterministic (no
// Math.random) so the screenshot is byte-stable across CI runs; a gentle sine
// gives the charts a realistic shape without a live capture.
const STAGE_BASE_US: Record<string, number> = {
	capture: 320,
	submit: 90,
	encode: 760,
	send: 140,
};
const STAGE_ORDER = ["capture", "submit", "encode", "send"];

function buildSamples(n: number): StatsSample[] {
	const out: StatsSample[] = [];
	for (let i = 0; i < n; i++) {
		const wobble = Math.sin(i / 4);
		out.push({
			t_ms: i * 1000,
			session_id: 1,
			fps: 240,
			repeat_fps: i % 3 === 0 ? 2 : 1,
			mbps: 920 + wobble * 55,
			bitrate_kbps: 150_000,
			frames_dropped: i % 17 === 0 ? 1 : 0,
			packets_dropped: i % 9 === 0 ? 2 : 0,
			send_dropped: 0,
			fec_recovered: i % 5 === 0 ? 3 : 1,
			stages: STAGE_ORDER.map((name) => {
				const base = STAGE_BASE_US[name] ?? 100;
				const p50 = Math.round(base + wobble * base * 0.15);
				return { name, p50_us: p50, p99_us: Math.round(p50 * 1.8) };
			}),
		});
	}
	return out;
}

export const captureMetas: CaptureMeta[] = [
	{
		id: "cap-20260628-2041",
		client: "enricos-macbook",
		kind: "native",
		codec: "hevc",
		width: 5120,
		height: 1440,
		fps: 240,
		duration_ms: 92_000,
		sample_count: 92,
		started_unix_ms: 1_782_415_260_000,
		encoder_backend: "nvenc",
		gpu: "NVIDIA GeForce RTX 4090",
	},
	{
		id: "cap-20260628-1903",
		client: "living-room-tv",
		kind: "gamestream",
		codec: "av1",
		width: 3840,
		height: 2160,
		fps: 120,
		duration_ms: 240_000,
		sample_count: 240,
		started_unix_ms: 1_782_409_380_000,
	},
];

export const captureDetail: Capture = {
	meta: captureMetas[0] as CaptureMeta,
	samples: buildSamples(60),
};

// --- Pairing page ------------------------------------------------------------

export const nativePairArmed: NativePairStatus = {
	enabled: true,
	armed: true,
	pin: "4827",
	expires_in_secs: 98,
	paired_clients: 2,
};

/** Fixed "now" for every access countdown in the stories — screenshots must not drift. */
export const accessNowUnix = 1_755_200_000;

export const pendingDevices: PendingDevice[] = [
	{
		id: 1,
		name: "studio-deck",
		fingerprint:
			"9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00",
		age_secs: 8,
	},
	{
		id: 2,
		name: "Mac Mini",
		fingerprint:
			"ff00eeddccbbaa998877665544332211009f8e7d6c5b4a39281706f5e4d3c2b1",
		age_secs: 30,
	},
];

/** The expired-guest re-knock: already known, stored as Controller only · 4 h (now past). */
export const pendingGuestReknock: PendingDevice = {
	id: 3,
	name: "leons-deck",
	fingerprint:
		"0011223344556677889900aabbccddeeff102030405060708090a0b0c0d0e0f0",
	age_secs: 12,
	access_level: "controller",
	grants: 0x01,
	granted_unix: accessNowUnix - 6 * 3600,
	expires_unix: accessNowUnix - 2 * 3600,
};

export const nativeClients: NativeClient[] = [
	{
		name: "enricos-macbook",
		fingerprint:
			"a1b2c3d4e5f60718293a4b5c6d7e8f90112233445566778899aabbccddeeff00",
		access_level: "full",
		grants: 0x3f,
		granted_unix: accessNowUnix - 30 * 86400,
		expires_unix: null,
	},
	{
		name: "living-room-tv",
		fingerprint:
			"ff00eeddccbbaa998877665544332211009f8e7d6c5b4a39281706f5e4d3c2b1",
		access_level: "controller",
		grants: 0x01,
		granted_unix: accessNowUnix - 2 * 3600,
		// 1 h 58 min out — renders as the design's "Controller · 2 h left".
		expires_unix: accessNowUnix + 7080,
	},
];

export const pairingIdle: PairingStatus = { pin_pending: false };

/** The six axes a preset expands to — the baseline the built-ins vary from. */
const policyFields = (
	over: Partial<EffectivePolicy> = {},
): EffectivePolicy => ({
	identity: "per-client",
	keep_alive: { mode: "duration", seconds: 300 },
	layout: { mode: "auto-row", positions: {} },
	max_displays: 4,
	mode_conflict: "separate",
	topology: "auto",
	...over,
});

/**
 * The built-in presets as `GET /display/settings` returns them. Summaries are the host's own prose
 * (it composes them from the fields), so they are literal strings here rather than i18n messages.
 */
export const displayPresets: {
	id: string;
	summary: string;
	fields: EffectivePolicy;
}[] = [
	{
		id: "default",
		summary:
			"A virtual display per client, released 5 minutes after it disconnects.",
		fields: policyFields(),
	},
	{
		id: "shared-desktop",
		summary:
			"Every client sees the same desktop — no extra displays are created.",
		fields: policyFields({
			identity: "shared",
			topology: "primary",
			mode_conflict: "join",
		}),
	},
	{
		id: "hotdesk",
		summary:
			"One display at a time; the physical heads go dark while you stream.",
		fields: policyFields({
			topology: "exclusive",
			max_displays: 1,
			keep_alive: { mode: "off" },
		}),
	},
	{
		id: "workstation",
		summary: "Adds a display beside the monitors already on the desk.",
		fields: policyFields({ topology: "extend" }),
	},
	{
		id: "gaming-rig",
		summary:
			"Pins the display so it survives every disconnect — free it with Release.",
		fields: policyFields({
			topology: "exclusive",
			keep_alive: { mode: "forever" },
		}),
	},
];

/** Two operator-saved bundles, so the custom-preset rail has something in it. */
export const displayCustomPresets: CustomPreset[] = [
	{
		id: "cp-couch",
		name: "Couch (TV only)",
		fields: policyFields({
			topology: "exclusive",
			max_displays: 1,
			keep_alive: { mode: "forever" },
		}),
		game_session: "dedicated",
	},
	{
		id: "cp-office",
		name: "Office desk",
		fields: policyFields({ topology: "extend", identity: "shared" }),
	},
];

/** What the host reports as in force — the `default` preset's expansion. */
export const displayEffective: EffectivePolicy = policyFields();

/** The stored policy: a plain built-in pick, which is what most hosts sit on. */
export const displayPolicy: DisplayPolicy = {
	preset: "default",
	game_session: "auto",
	version: 1,
};
