// A plugin's UI, embedded in the console (plugin-ui-surface §5). We probe the plugin's liveness
// first and only mount the iframe when it answers — otherwise the iframe would show the proxy's raw
// 502. The iframe is same-origin (proxied through /plugin-ui), so the plugin can talk to its own
// loopback REST with the operator's session and, optionally, keep the address bar in sync by posting
// `{ type: "pf-ui:navigate", path }` to the parent.
import { useQuery } from "@tanstack/react-query";
import { getRouteApi, useNavigate } from "@tanstack/react-router";
import { ExternalLink, RefreshCw } from "lucide-react";
import { type FC, useEffect, useMemo, useRef } from "react";
import { pluginIcon, usePlugins } from "@/api/plugins";
import { useInstalledPlugins } from "@/api/store";
import { Button } from "@/components/ui/button";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { TierBadge } from "@/sections/Store/TierBadge";

const route = getRouteApi("/plugins/$pluginId/$");

export const SectionPlugin: FC = () => {
	useLocale();
	const { pluginId, _splat } = route.useParams();
	const navigate = useNavigate();
	const iframeRef = useRef<HTMLIFrameElement>(null);

	// Header metadata (title/version/icon) from the directory; falls back to the id.
	const { data: plugins } = usePlugins();
	const meta = plugins?.find((p) => p.id === pluginId);
	const Icon = pluginIcon(meta?.ui?.icon);
	const title = meta?.title ?? pluginId;

	// Provenance follows the plugin into its own page: an unverified plugin must stay visibly
	// unverified WHILE you use it, not only in the store listing. The store keys installations by
	// package, so the runtime id is matched through `plugin_id`.
	const { data: installed } = useInstalledPlugins();
	const provenance = installed?.find((p) => p.plugin_id === pluginId);

	// Liveness: a 200 from /__health means the plugin is up.
	//
	// Two subtleties, both learned the hard way:
	//
	//  - A 200 is not enough. `fetch` follows redirects, so an expired session — where the gate
	//    answers 302 → /login → 200 HTML — looked exactly like a healthy plugin, and the console
	//    rendered its own login page inside the plugin's iframe. `redirect: "manual"` makes that
	//    an opaque response we can reject instead.
	//  - One failure must not be terminal. The runner is restarted at the end of every successful
	//    install, so a single missed probe is routine; giving up on the first one threw away
	//    whatever the operator had open in another plugin. Retry a few times, and keep probing on a
	//    slower beat while down so it recovers on its own.
	const health = useQuery({
		queryKey: ["plugin-health", pluginId],
		queryFn: async () => {
			const r = await fetch(`/plugin-ui/${pluginId}/__health`, {
				credentials: "same-origin",
				redirect: "manual",
			});
			// `type === "opaqueredirect"` is the gate bouncing us to /login, not the plugin answering.
			if (r.type === "opaqueredirect") throw new Error("session expired");
			if (!r.ok) throw new Error(`health ${r.status}`);
			return true;
		},
		retry: 2,
		refetchInterval: (q) => (q.state.status === "error" ? 5_000 : 20_000),
	});

	// The iframe src is fixed at the initial deep-link path; the plugin's own in-app navigation drives
	// the console URL via postMessage (below), never the src — so there's no reload loop.
	// biome-ignore lint/correctness/useExhaustiveDependencies: intentionally pinned to the initial path
	const initialSrc = useMemo(
		() => `/plugin-ui/${pluginId}/${_splat ?? ""}`,
		[pluginId],
	);

	// Keep the console address bar in sync with the plugin's internal routing.
	useEffect(() => {
		const onMessage = (e: MessageEvent) => {
			if (e.source !== iframeRef.current?.contentWindow) return;
			const data = e.data as { type?: string; path?: string };
			if (data?.type === "pf-ui:navigate" && typeof data.path === "string") {
				navigate({
					to: "/plugins/$pluginId/$",
					params: { pluginId, _splat: data.path.replace(/^\//, "") },
					replace: true,
				});
			}
		};
		window.addEventListener("message", onMessage);
		return () => window.removeEventListener("message", onMessage);
	}, [pluginId, navigate]);

	return (
		<div className="flex h-[calc(100dvh-7rem)] min-h-[480px] flex-col gap-3 sm:h-[calc(100dvh-5rem)]">
			{/* Header strip: identity + open-in-new-tab (the plugin stands alone full-window too). */}
			<div className="flex items-center gap-3">
				<Icon className="size-5 text-muted-foreground" />
				<h1 className="text-lg font-semibold">{title}</h1>
				{meta?.version && (
					<span className="rounded bg-muted px-1.5 py-0.5 text-xs text-muted-foreground">
						v{meta.version}
					</span>
				)}
				{provenance && <TierBadge tier={provenance.tier} />}
				<a
					href={`/plugin-ui/${pluginId}/`}
					target="_blank"
					rel="noreferrer"
					className="ml-auto inline-flex items-center gap-1.5 text-sm text-muted-foreground transition-colors hover:text-foreground"
				>
					<ExternalLink className="size-4" />
					{m.plugin_open_new_tab()}
				</a>
			</div>

			{health.isError ? (
				<OfflineCard title={title} onRetry={() => health.refetch()} />
			) : health.isSuccess ? (
				<iframe
					ref={iframeRef}
					src={initialSrc}
					title={title}
					className="w-full flex-1 rounded-lg border bg-card"
					// The plugin is operator-installed code on our own origin (no new trust boundary —
					// plugin-ui-surface §7.4); allow it to run scripts, forms, popups, and full-window.
					sandbox="allow-scripts allow-forms allow-popups allow-same-origin allow-modals"
					allow="fullscreen"
				/>
			) : (
				// Probing: a calm shimmer rather than a flash of empty frame.
				<div className="flex-1 animate-pulse rounded-lg border bg-card/50" />
			)}
		</div>
	);
};

const OfflineCard: FC<{ title: string; onRetry: () => void }> = ({
	title,
	onRetry,
}) => (
	<div className="flex flex-1 items-center justify-center rounded-lg border border-dashed">
		<div className="flex max-w-md flex-col items-center gap-3 p-8 text-center">
			<h2 className="text-base font-semibold">{m.plugin_offline_title()}</h2>
			<p className="text-sm text-muted-foreground">
				<span className="font-medium text-foreground">{title}</span> —{" "}
				{m.plugin_offline_hint()}
			</p>
			{/* The exact runner commands, so the operator can act without leaving the page. */}
			<pre className="w-full overflow-x-auto rounded-md bg-muted p-3 text-left text-xs text-muted-foreground">
				<code>
					systemctl --user status punktfunk-scripting{"\n"}
					Get-ScheduledTask PunktfunkScripting{"  # Windows"}
				</code>
			</pre>
			<Button variant="outline" size="sm" onClick={onRetry}>
				<RefreshCw className="size-4" />
				{m.plugin_retry()}
			</Button>
		</div>
	</div>
);
