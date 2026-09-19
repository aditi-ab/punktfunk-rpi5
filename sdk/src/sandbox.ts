// One sandbox per plugin (design/host-trust-boundaries.md §3.1).
//
// The runner is a `systemd --user` unit, so it shares the operator's uid — and a mount namespace
// alone is no boundary against the same uid: `/proc/<host pid>/root` reaches the files it hides,
// and `/proc/<pid>/environ` and `kill` pass the same check. What makes this one hold is the pid
// namespace: with `--unshare-pid` and a fresh `/proc`, the host's process does not exist inside,
// so there is nothing to reach through.
//
// Each plugin gets: an empty home, its own state dir, the paths its manifest declares, its own
// token, and a unix socket to the host. No network unless it declared one. Pin: `sandbox.test.ts`.
import { spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

/** A plugin's `punktfunk` block — the half of it a sandbox is built from. */
export interface PluginManifest {
	schema?: number;
	id?: string;
	reads?: string[];
	writes?: string[];
	network?: boolean;
}

/** Read `package.json`'s `punktfunk` block, or `undefined` when there is none. */
export const readManifest = (packageDir: string): PluginManifest | undefined => {
	try {
		const pkg = JSON.parse(
			fs.readFileSync(path.join(packageDir, "package.json"), "utf8"),
		) as { punktfunk?: PluginManifest };
		const m = pkg.punktfunk;
		return m && m.schema === 1 && typeof m.id === "string" ? m : undefined;
	} catch {
		return undefined;
	}
};

/** `~/x` → `<home>/x`. A path without the prefix is already absolute or is skipped. */
export const expandHome = (p: string, home: string): string =>
	p.startsWith("~/") ? path.join(home, p.slice(2)) : p;

/** One extra root the operator granted a plugin, read-only unless `write` is set. */
export interface GrantedRoot {
	path: string;
	write: boolean;
}

export interface SandboxPaths {
	/** Where this plugin's own files live, bound read-write. */
	stateDir: string;
	/** The file holding this plugin's token, bound read-only as its `plugin-token`. */
	tokenFile: string;
	/** The unix socket the supervisor proxies to the host on. */
	socket: string;
	/** The plugin install root (`<config>/plugins`), bound read-only: its code. */
	pluginsDir: string;
	/** The runtime and the runner bundle the child re-execs. */
	bun: string;
	runner: string;
	home: string;
}

/**
 * The `bwrap` argv for one plugin: everything before the program it runs.
 *
 * Read-only for the system, the plugin's own code, manifest `reads`, and granted roots; read-write
 * for exactly its state dir, manifest `writes`, grants marked `write: true`, and `/tmp` (VirtualHere's
 * client IPC is a FIFO pair there, which is why the unit keeps the real `/tmp`).
 */
/**
 * The namespaces and the minimal root every sandbox gets, shared with {@link sandboxProbe} so a
 * box is never called capable on flags the real sandbox does not use — and so a flag added here
 * is a flag the probe actually exercises.
 *
 * The `/lib` + `/lib64` symlinks are not cosmetic: with only `/usr` bound, the dynamic loader is
 * absent and every exec fails as ENOENT, which reads exactly like a refused namespace.
 */
const BASE_ARGV: readonly string[] = [
	"--unshare-all",
	// `--unshare-all` only asks for a user namespace (`--unshare-user-try`), and
	// `--disable-userns` refuses to run without a real one. Demand it explicitly.
	"--unshare-user",
	// A plugin cannot re-enter this and build itself a wider one.
	"--disable-userns",
	"--die-with-parent",
	"--new-session",
	"--clearenv",
	// `--unshare-pid` is what makes the rest hold: a fresh /proc has no host process in it.
	"--proc",
	"/proc",
	// The unit cannot keep tunables read-only (ProtectKernelTunables refuses the mount above).
	"--ro-bind",
	"/proc/sys",
	"/proc/sys",
	"--dev",
	"/dev",
	"--tmpfs",
	"/tmp",
	"--ro-bind",
	"/usr",
	"/usr",
	"--ro-bind-try",
	"/etc/ssl",
	"/etc/ssl",
	"--ro-bind-try",
	"/etc/resolv.conf",
	"/etc/resolv.conf",
	"--symlink",
	"usr/lib",
	"/lib",
	"--symlink",
	"usr/lib64",
	"/lib64",
	"--symlink",
	"usr/bin",
	"/bin",
	"--symlink",
	"usr/sbin",
	"/sbin",
];

export const bwrapArgv = (
	manifest: PluginManifest,
	paths: SandboxPaths,
	grants: readonly GrantedRoot[] = [],
): string[] => {
	// The spawner hands `netlinkFilter()` over on fd 3.
	const argv = [...BASE_ARGV, "--add-seccomp-fd", "3"];
	// `--clearenv` empties the environment the child sees, so the spawn env never reaches it:
	// every value has to be re-stated here. Without this a plugin has no HOME, cannot resolve
	// its state dir, and cannot find the socket it reaches the host on.
	for (const [k, v] of Object.entries(sandboxEnv(paths.home))) {
		argv.push("--setenv", k, v);
	}
	if (manifest.network) argv.push("--share-net");
	// Its own code, its own state, its own token, and the way to the host.
	argv.push("--ro-bind", paths.pluginsDir, paths.pluginsDir);
	argv.push("--ro-bind", paths.bun, paths.bun);
	argv.push("--ro-bind", paths.runner, paths.runner);
	argv.push("--bind", paths.stateDir, "/run/punktfunk/plugin-state");
	argv.push("--ro-bind", paths.tokenFile, "/run/punktfunk/plugin-token");
	argv.push("--bind", paths.socket, "/run/punktfunk/host.sock");
	// What it said it needs. `-try` so an uninstalled launcher's dir is simply absent rather
	// than a sandbox that refuses to start.
	for (const p of manifest.reads ?? []) {
		const abs = expandHome(p, paths.home);
		if (path.isAbsolute(abs)) argv.push("--ro-bind-try", abs, abs);
	}
	for (const p of manifest.writes ?? []) {
		const abs = expandHome(p, paths.home);
		if (path.isAbsolute(abs)) argv.push("--bind-try", abs, abs);
	}
	// Operator grants are read-only unless one opts into write.
	for (const grant of grants) {
		const abs = expandHome(grant.path, paths.home);
		if (path.isAbsolute(abs))
			argv.push(grant.write ? "--bind-try" : "--ro-bind-try", abs, abs);
	}
	return argv;
};

/**
 * A seccomp program refusing netlink sockets, as the bytes bwrap reads from `--add-seccomp-fd`.
 *
 * The runner's unit allows AF_NETLINK only because bwrap needs it to bring up the sandbox's
 * loopback; bwrap loads this after that setup, so the plugin never gets it. Classic BPF over
 * `seccomp_data`: a foreign architecture or any x32 call is refused outright, then `socket` with
 * a netlink domain. `undefined` on an architecture with no table entry.
 */
export const netlinkFilter = (arch: string = process.arch): Buffer | undefined => {
	// AUDIT_ARCH_* and __NR_socket.
	const native = ({ x64: [0xc000003e, 41], arm64: [0xc00000b7, 198] } as const)[
		arch as "x64" | "arm64"
	];
	if (!native) return undefined;
	const [LOAD, JEQ, JGE, RET] = [0x20, 0x15, 0x35, 0x06];
	const DENY = 0x00050000 | 97; // SECCOMP_RET_ERRNO | EAFNOSUPPORT
	const ALLOW = 0x7fff0000;
	// [code, jump-if-true, jump-if-false, k]; a jump skips that many instructions.
	const program = [
		[LOAD, 0, 0, 4], // arch
		[JEQ, 0, 5, native[0]],
		[LOAD, 0, 0, 0], // syscall number
		[JGE, 3, 0, 0x40000000], // x32
		[JEQ, 0, 3, native[1]],
		[LOAD, 0, 0, 16], // low word of the first argument: the domain
		[JEQ, 0, 1, 16], // AF_NETLINK
		[RET, 0, 0, DENY],
		[RET, 0, 0, ALLOW],
	];
	const out = Buffer.alloc(program.length * 8);
	program.forEach(([code, jt, jf, k], i) => {
		out.writeUInt16LE(code, i * 8);
		out.writeUInt8(jt, i * 8 + 2);
		out.writeUInt8(jf, i * 8 + 3);
		out.writeUInt32LE(k, i * 8 + 4);
	});
	return out;
};

/** The environment inside: no inherited values, and nothing that is not needed there. */
export const sandboxEnv = (
	home: string,
	extra: Record<string, string> = {},
): Record<string, string> => ({
	// The REAL home, which is where a `~/...` read is bound. A plugin finds its declared paths
	// with os.homedir(); pointing this at the state dir sends every scanner somewhere empty.
	// Writable state is PUNKTFUNK_CONFIG_DIR's job, not this one's.
	HOME: home,
	PATH: "/usr/bin:/bin:/usr/local/bin",
	PUNKTFUNK_CONFIG_DIR: "/run/punktfunk",
	// Reached through the supervisor's socket, so plain HTTP with no credential of its own.
	PUNKTFUNK_MGMT_URL: "http://punktfunk.host",
	PUNKTFUNK_MGMT_UNIX: "/run/punktfunk/host.sock",
	...extra,
});

/**
 * The operator's extra roots for `id` from `plugin-grants.json`. Accepts a legacy path array
 * (read-only grants) and a `{ grants: [{ path, write }] }` record. Anything else — malformed JSON,
 * a missing entry, a shape that does not parse strictly — grants nothing.
 */
export const grantedRoots = (configDir: string, id: string): GrantedRoot[] => {
	try {
		const map = JSON.parse(
			fs.readFileSync(path.join(configDir, "plugin-grants.json"), "utf8"),
		) as Record<string, unknown>;
		const entry = map[id];
		if (Array.isArray(entry)) {
			if (!entry.every((p) => typeof p === "string")) return [];
			return entry.map((p) => ({ path: p, write: false }));
		}
		if (typeof entry !== "object" || entry === null) return [];
		const grants = (entry as { grants?: unknown }).grants;
		if (!Array.isArray(grants)) return [];
		if (
			!grants.every(
				(g) =>
					typeof g === "object" &&
					g !== null &&
					!Array.isArray(g) &&
					typeof (g as GrantedRoot).path === "string" &&
					typeof (g as GrantedRoot).write === "boolean",
			)
		)
			return [];
		return grants as GrantedRoot[];
	} catch {
		return [];
	}
};

/** Can this box sandbox at all? The reason, when it cannot, is what the operator needs. */
export const sandboxProbe = (
	run: (cmd: string, args: string[]) => { status: number | null } = (cmd, args) =>
		spawnSync(cmd, args, { stdio: "ignore" }),
	platform: string = process.platform,
): { ok: true } | { ok: false; reason: string } => {
	if (platform !== "linux") {
		return { ok: false, reason: "sandboxing is Linux-only here" };
	}
	// The namespace flags `bwrapArgv` actually uses. Probing a weaker set reports a box as
	// sandbox-capable that then refuses every plugin.
	// Exactly what a real sandbox asks for, plus a trivial exec. Probing a weaker set reports a
	// box as capable that then refuses every plugin.
	const probe = run("bwrap", [...BASE_ARGV, "/bin/true"]);
	if (probe.status === 0) return { ok: true };
	if (probe.status === null) {
		return {
			ok: false,
			reason:
				"bubblewrap (bwrap) is not installed — install it, or set PUNKTFUNK_PLUGIN_SANDBOX=off",
		};
	}
	return {
		ok: false,
		reason:
			"bwrap could not create a namespace — this kernel restricts unprivileged user namespaces",
	};
};
