// @punktfunk/plugin-kit — Effect-based framework for punktfunk plugins.
export * from "./errors.js";
export {
	atomicWriteFile,
	ensureStateDir,
	pluginIngestDir,
	pluginStateDir,
	statePath,
} from "./paths.js";
export {
	HostClient,
	hostClientFromFacade,
	type HostClientService,
	PluginInfo,
	pluginInfoLayer,
	type PluginInfoService,
} from "./host-client.js";
export { loggingLayer } from "./logging.js";
export { type ConfigService, makeConfigService } from "./config.js";
export { type CacheStore, makeCacheStore } from "./cache-store.js";
export {
	Artwork,
	DetectHint,
	GameMeta,
	LaunchSpec,
	PrepStep,
	ProviderClient,
	type ProviderClientService,
	ProviderEntry,
} from "./reconcile.js";
export {
	definePluginKit,
	type PluginKitDef,
	runPluginKitDirect,
} from "./runtime.js";
export {
	type LastSync,
	makeSyncEngine,
	type SyncEngine,
	type SyncEngineOptions,
	type SyncOutcome,
	type SyncReason,
	type SyncSettings,
	type SyncStatus,
} from "./sync-engine.js";
export { httpApiEnv, serveUi, type ServeUiOptions } from "./ui-server.js";
export { sseRoute, type SseRouteOptions } from "./sse.js";
export { type CliCommand, runPluginCli } from "./cli.js";
