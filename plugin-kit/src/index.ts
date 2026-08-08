// @punktfunk/plugin-kit — Effect-based framework for punktfunk plugins.

export { type CacheStore, makeCacheStore } from "./cache-store.js";
export { type CliCommand, runPluginCli } from "./cli.js";
export { type ConfigService, makeConfigService } from "./config.js";
export * from "./errors.js";
export {
	HostClient,
	type HostClientService,
	hostClientFromFacade,
	PluginInfo,
	type PluginInfoService,
	pluginInfoLayer,
} from "./host-client.js";
export { loggingLayer } from "./logging.js";
export {
	atomicWriteFile,
	ensureStateDir,
	pluginIngestDir,
	pluginStateDir,
	statePath,
} from "./paths.js";
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
export { type SseRouteOptions, sseRoute } from "./sse.js";
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
export {
	deriveConfigJsonSchema,
	httpApiEnv,
	makeConfigHandler,
	makeLaunchHandler,
	type PluginLaunchTarget,
	type ServeUiConfig,
	type ServeUiOptions,
	serveUi,
} from "./ui-server.js";
