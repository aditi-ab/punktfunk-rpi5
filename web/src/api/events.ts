// The host's event stream, wired to React Query's cache.
//
// The host publishes every lifecycle transition on `GET /api/v1/events` as SSE — client
// connect/disconnect, session and stream start/end, pairing decisions, display create/release,
// library/store/plugin changes, update availability, host start/stop. Nothing consumed it: the
// console learned about all of it by asking again on ten separate timers, so a change was up to
// 5 s stale, two pages could disagree with each other while you looked at them, and the Library
// page — which polls not at all — never noticed a newly installed game until a full reload.
//
// This subscribes once for the whole app and invalidates exactly the queries an event affects.
// It does NOT carry data into the cache: the REST snapshots stay the source of truth, and an event
// only says "this is stale now". That keeps the wire format additive-only (a kind we don't know
// costs us nothing) and means a missed event degrades to the polling behaviour we already had.
//
// Transport notes:
//   - Same-origin, so the sealed session cookie rides along and the BFF injects the mgmt bearer;
//     no auth work here. `EventSource` reconnects on its own and replays with `Last-Event-ID`,
//     which h3's proxy forwards, so a dropped connection resumes from the host's ring.
//   - The host sends a keep-alive comment every 15 s; the Bun entry's idle timeout is set above
//     that (nitro-entry/bun-https.mjs) so we don't sever our own stream.
//   - An `event: dropped` frame means we fell off the ring and must resync — invalidate everything.
import { type QueryClient, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { useEffect, useSyncExternalStore } from "react";
import { getListPairedClientsQueryKey } from "@/api/gen/clients/clients";
import { getGetDisplayStateQueryKey } from "@/api/gen/display/display";
import { getGetStatusQueryKey } from "@/api/gen/host/host";
import { getGetLibraryQueryKey } from "@/api/gen/library/library";
import {
	getListNativeClientsQueryKey,
	getListPendingDevicesQueryKey,
} from "@/api/gen/native/native";
import { getGetPairingStatusQueryKey } from "@/api/gen/pairing/pairing";
import { getGetUpdateStatusQueryKey } from "@/api/gen/update/update";
import { boostPluginPolling, PLUGINS_KEY } from "@/api/plugins";
import { storeKeys } from "@/api/store";
import { m } from "@/paraglide/messages";

/** Which query keys a given event kind invalidates. Unknown kinds are ignored on purpose.
 * (The generated key helpers return `readonly` tuples, which is what React Query wants.) */
function keysFor(kind: string): readonly (readonly unknown[])[] {
	const status = [getGetStatusQueryKey()];
	switch (kind) {
		// Anything that changes what the host is doing right now moves the dashboard's status.
		case "client.connected":
		case "client.disconnected":
		case "session.started":
		case "session.ended":
		case "stream.started":
		case "stream.stopped":
		case "game.running":
		case "game.exited":
			return status;
		// A display appearing or going away changes the live list, and its policy card shows
		// "in effect" values derived from the same state.
		case "display.created":
		case "display.released":
			return [...status, getGetDisplayStateQueryKey()];
		// A knock (and a denial clearing one) changes the pending list itself — which is the one
		// an operator sits and waits on. It used to refresh only on its own 10 s timer.
		case "pairing.pending":
		case "pairing.denied":
			return [
				...status,
				getGetPairingStatusQueryKey(),
				getListPendingDevicesQueryKey(),
			];
		// A completed pairing also adds a device to whichever plane's list is on screen.
		case "pairing.completed":
			return [
				...status,
				getGetPairingStatusQueryKey(),
				getListPairedClientsQueryKey(),
				getListNativeClientsQueryKey(),
			];
		// The base key with no params is a PREFIX of every parameterised library query, and React
		// Query invalidates by prefix — so this catches the Dashboard's and the Library page's alike.
		case "library.changed":
			return [getGetLibraryQueryKey()];
		case "update.available":
		case "update.applied":
			return [getGetUpdateStatusQueryKey()];
		// A plugin install/uninstall moves the nav, the catalog, and the installed list.
		case "plugins.changed":
		case "store.changed":
			return [
				PLUGINS_KEY,
				storeKeys.catalog,
				storeKeys.installed,
				storeKeys.runtime,
			];
		// The host came back: everything we hold predates it.
		case "host.started":
			return [];
		default:
			return [];
	}
}

/**
 * Mark one key's data wrong and refetch it.
 *
 * `refetchType: "all"` rather than the default `"active"`: an event says the HOST changed, so every
 * cached copy is wrong, whether or not a mounted component happens to be observing it right now.
 * The default only refetches queries with a live observer, which silently did nothing for a page
 * that had just been re-rendered — the cache stayed marked-stale-but-unfetched and the screen kept
 * showing the old answer.
 */
function invalidate(qc: QueryClient, queryKey: readonly unknown[]): void {
	qc.invalidateQueries({ queryKey, refetchType: "all" });
}

/** Invalidate every query — used on `dropped` (we fell off the ring) and on `host.started`. */
function resyncAll(qc: QueryClient): void {
	qc.invalidateQueries({ refetchType: "all" });
}

// ---------------------------------------------------------------------------------------------
// The activity log.
//
// The same frames that drive invalidation are also, in themselves, the answer to "what has this
// host been doing?" — a question the console could not answer at all. Nothing else records this:
// the REST snapshots describe the present, and the host's own log is a developer artifact, not a
// narrative. So keep a small in-memory ring alongside the cache work.
//
// Deliberately NOT persisted and deliberately bounded: it is a live tail for someone watching, not
// an audit trail, and a page load starts fresh from whatever the ring replays.
// ---------------------------------------------------------------------------------------------

/** One thing that happened, as the feed renders it. */
export interface ActivityEntry {
	/** The host's monotonic sequence number — stable, and a good React key. */
	seq: number;
	/** Unix ms, from the host's clock (never the browser's). */
	ts_ms: number;
	kind: string;
	/** The event payload, shape depending on `kind` (see the EventKind schema). */
	data: Record<string, unknown>;
}

const ACTIVITY_MAX = 200;
let activity: ActivityEntry[] = [];
const activityListeners = new Set<() => void>();

function pushActivity(entry: ActivityEntry): void {
	// Guard against a replayed frame after a reconnect (`Last-Event-ID` can re-deliver the cursor).
	if (activity.some((e) => e.seq === entry.seq)) return;
	activity = [entry, ...activity].slice(0, ACTIVITY_MAX);
	for (const l of activityListeners) l();
}

/** The activity tail, newest first. Re-renders as frames arrive. */
export function useActivity(): ActivityEntry[] {
	return useSyncExternalStore(
		(cb) => {
			activityListeners.add(cb);
			return () => activityListeners.delete(cb);
		},
		() => activity,
		// The server has no stream, so SSR renders an empty feed and hydrates into the live one.
		() => EMPTY_ACTIVITY,
	);
}

const EMPTY_ACTIVITY: ActivityEntry[] = [];

/** Every kind we act on. A kind the host adds later simply has no listener — never a mis-handle. */
const KINDS = [
	"client.connected",
	"client.disconnected",
	"session.started",
	"session.ended",
	"stream.started",
	"stream.stopped",
	"game.running",
	"game.exited",
	"pairing.pending",
	"pairing.completed",
	"pairing.denied",
	"display.created",
	"display.released",
	"library.changed",
	"update.available",
	"update.applied",
	"plugins.changed",
	"store.changed",
	"host.started",
] as const;

// ---------------------------------------------------------------------------------------------
// The connection is a module-level singleton, refcounted, NOT a per-component resource.
//
// It has to be. The subscription is app-lifetime, but the component that asks for it is not:
// during hydration TanStack Start mounts the app shell and discards it again ~15 ms later
// (measured), which ran an effect cleanup with no matching re-mount. Tied to that effect, the
// stream opened, closed, and never came back — the console looked subscribed and received nothing.
//
// So: `open()` hands out a reference and only the LAST release closes the socket, after a short
// grace period, so a remount inside that window re-attaches to the live stream instead of
// reconnecting. `EventSource` handles reconnection itself and replays with `Last-Event-ID`, which
// the SSE route forwards.
// ---------------------------------------------------------------------------------------------
let source: EventSource | null = null;
let refs = 0;
let closeTimer: ReturnType<typeof setTimeout> | null = null;
/** The client to invalidate against — one per page load, re-pointed if React hands us a new one. */
let client: QueryClient | null = null;

/** How long the stream survives with no subscribers, so a hydration blip doesn't reconnect. */
const CLOSE_GRACE_MS = 10_000;

function attach(): void {
	if (source) return;
	source = new EventSource("/api/v1/events");
	for (const kind of KINDS) {
		source.addEventListener(kind, (ev) => {
			// Record it first: the feed should show an event even for a kind we invalidate nothing for.
			const entry = recordActivity(kind, ev);
			// A knock needs someone to act on it, and it can land on any page — so it is the one
			// event that interrupts rather than waiting to be noticed on the Pairing page.
			if (kind === "pairing.pending") announceKnock(entry);
			if (!client) return;
			// The installed set changed — but the runner is probably still restarting, so keep
			// checking for a while rather than trusting this one refetch (see boostPluginPolling).
			if (kind === "plugins.changed" || kind === "store.changed")
				boostPluginPolling();
			for (const key of keysFor(kind)) invalidate(client, key);
			// `host.started` names no keys — the host is NEW, so everything we hold predates it.
			if (kind === "host.started") resyncAll(client);
		});
	}
	// We fell off the host's ring — every snapshot we hold may be wrong.
	source.addEventListener("dropped", () => {
		if (client) resyncAll(client);
	});
}

/**
 * Parse one SSE frame into the activity ring, and hand back what it parsed so a caller can read
 * the payload without parsing the same frame twice. A malformed frame is dropped, never thrown.
 */
function recordActivity(kind: string, ev: Event): ActivityEntry | null {
	const raw = (ev as MessageEvent<string>).data;
	if (typeof raw !== "string") return null;
	try {
		const data = JSON.parse(raw) as Record<string, unknown>;
		const seq = typeof data.seq === "number" ? data.seq : Number.NaN;
		const ts = typeof data.ts_ms === "number" ? data.ts_ms : Number.NaN;
		if (!Number.isFinite(seq) || !Number.isFinite(ts)) return null;
		const entry: ActivityEntry = { seq, ts_ms: ts, kind, data };
		pushActivity(entry);
		return entry;
	} catch {
		// A frame we cannot parse is not worth breaking the stream over.
		return null;
	}
}

/**
 * Tell the operator a device is knocking, wherever they are in the console.
 *
 * The pending list is on one page, and until now nothing said a request had arrived unless you
 * were already looking at it — worst on a phone, where that page is several taps away.
 *
 * The action is a plain navigation rather than a router push: this module is not a component and
 * has no router to reach. It costs a page load, on a button someone pressed on purpose.
 */
function announceKnock(entry: ActivityEntry | null): void {
	const device = entry?.data.device as { name?: unknown } | undefined;
	const name = typeof device?.name === "string" ? device.name : null;
	toast.info(m.pairing_knock_title(), {
		description: name ? m.pairing_knock_desc({ name }) : undefined,
		action: {
			label: m.pairing_knock_action(),
			onClick: () => window.location.assign("/pairing"),
		},
	});
}

function release(): void {
	refs -= 1;
	if (refs > 0) return;
	if (closeTimer) clearTimeout(closeTimer);
	closeTimer = setTimeout(() => {
		closeTimer = null;
		if (refs > 0) return; // someone re-subscribed inside the grace window
		source?.close();
		source = null;
	}, CLOSE_GRACE_MS);
}

/**
 * Subscribe to the host's event stream. Safe to call from more than one component and safe on the
 * server (`EventSource` is browser-only, so this is a no-op during SSR).
 */
export function useHostEvents(): void {
	const qc = useQueryClient();
	useEffect(() => {
		if (typeof window === "undefined" || typeof EventSource === "undefined")
			return;
		client = qc;
		refs += 1;
		if (closeTimer) {
			clearTimeout(closeTimer);
			closeTimer = null;
		}
		attach();
		return release;
	}, [qc]);
}
