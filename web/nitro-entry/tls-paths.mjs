// Which of the host's two identities the console serves — resolved HERE because this entry is the
// one place every launcher routes through.
//
// The host keeps two identities side by side (crate::identity, the "identity split"):
//
//   native-cert.pem / native-key.pem  ECDSA P-256, with real SANs (the machine hostname,
//                                     localhost, 127.0.0.1, ::1). This is what the native QUIC
//                                     plane and the management API present, and what native
//                                     clients pin.
//   cert.pem / key.pem                the legacy RSA GameStream identity: CN=punktfunk and NO SAN
//                                     at all (gamestream::cert::generate passes rcgen an empty SAN
//                                     list), kept byte-stable because Moonlight pins it and the
//                                     pairing hashes bind its X.509 signature bytes.
//
// Every launcher names the LEGACY pair — scripts/punktfunk-web.service, the NixOS module, the
// Windows service supervisor, web-run.cmd, the Steam Deck installer — because they were written
// before the split, and none of them CAN choose: systemd `Environment=` has no "this file, else
// that one". Serving the legacy pair costs twice:
//
//   * a CN-only, SAN-less cert is rejected outright by every current browser
//     (ERR_CERT_COMMON_NAME_INVALID / SSL_ERROR_BAD_CERT_DOMAIN), so the console the operator was
//     told to open does not load;
//   * the tray's loopback liveness probe pins whatever the mgmt API serves — the NATIVE cert — so
//     the handshake is refused and a perfectly healthy console is labelled "Open web console (not
//     responding)" while the host beside it reads "idle" (field report 2026-08-24).
//
// So prefer the native sibling. It is also the smaller secret to hand a bundled bun: on a default
// build key.pem is the Moonlight PAIRING SIGNING key, native-key.pem is only a TLS key.
//
// Swapped as a PAIR or not at all — a native cert with the legacy key is a server that cannot
// complete a handshake with anyone. A host that never took the split (upgraded, native clients
// still pinning the RSA cert, so `load_or_adopt` keeps serving it) has no native pair on disk and
// falls through unchanged, as does a cert an operator supplied under any other name.
import { existsSync } from "node:fs";
import { basename, dirname, join } from "node:path";

/**
 * @param {string | undefined} cert  PUNKTFUNK_UI_TLS_CERT, verbatim.
 * @param {string | undefined} key   PUNKTFUNK_UI_TLS_KEY, verbatim.
 * @param {(p: string) => boolean} [exists]  injected by the test; defaults to a real stat.
 * @returns {{cert: string | undefined, key: string | undefined}}
 */
export function resolveUiTlsPaths(cert, key, exists = existsSync) {
	// Half-configured TLS is the caller's error to report (it refuses to start); don't mask it by
	// resolving one half of a pair that isn't there.
	if (!cert || !key) return { cert, key };
	if (basename(cert) !== "cert.pem" || basename(key) !== "key.pem") {
		return { cert, key };
	}
	const nativeCert = join(dirname(cert), "native-cert.pem");
	const nativeKey = join(dirname(key), "native-key.pem");
	return exists(nativeCert) && exists(nativeKey)
		? { cert: nativeCert, key: nativeKey }
		: { cert, key };
}
