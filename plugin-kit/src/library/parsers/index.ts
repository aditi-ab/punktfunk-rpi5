// The launcher-file parsing toolkit: what the six in-host scanners hand-rolled, hoisted so a
// library plugin is its scan function and nothing else.
//
// Everything here is total — a missing launcher, a truncated file, a schema drift in a launcher
// upgrade all degrade to "no titles from this source", never to a thrown error. A scanner that dies
// on one odd file takes the user's whole library with it.
export {
	ART_KINDS,
	type ArtKind,
	fileUrl,
	findGridArtFile,
	findLocalArtFile,
	gridFilenames,
	steamCdnUrl,
} from "./art.js";
export {
	confinedJoin,
	isDir,
	isFile,
	listDir,
	MAX_CACHE_BYTES,
	MAX_MANIFEST_BYTES,
	readBytesCapped,
	readJsonCapped,
	readTextCapped,
} from "./fs.js";
export {
	type FetchedBytes,
	type FetchLimits,
	fetchBytes,
	fetchJson,
} from "./http.js";
export {
	parseRegQuery,
	regQueryValue,
	regQueryValues,
	regSubKeys,
	type RegValue,
	validRegKey,
} from "./registry.js";
export {
	crc32,
	parseShortcuts,
	type Shortcut,
	shortcutAppId,
	shortcutGameId,
} from "./shortcuts.js";
export {
	steamLibraryDirs,
	steamRoots,
	steamUserConfigDirs,
} from "./steam-root.js";
export {
	type AppManifest,
	isSteamTool,
	parseAppManifest,
	vdfField,
	vdfPaths,
	vdfValue,
} from "./vdf.js";
