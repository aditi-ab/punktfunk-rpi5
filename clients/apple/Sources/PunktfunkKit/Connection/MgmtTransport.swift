// HTTPS transport for the host's management REST API, built on Network.framework rather than
// URLSession.
//
// WHY NOT URLSession. App Transport Security governs the URL loading system, and its default
// policy exempts only "local" destinations — `.local` names, unqualified names, and RFC1918 /
// link-local IP literals. Everything else must present a certificate that passes system trust
// evaluation. A punktfunk host is self-signed by construction (there is no CA that could vouch
// for a box on someone's LAN), so the library worked at 192.168.x and died at the TLS layer on
// every other address: a Tailscale peer (100.64/10 is CGNAT, NOT RFC1918), a WireGuard peer, or a
// public IP. No ATS key can express "any address the user typed" — the exception keys are
// domain-scoped — so the only ways out were disabling ATS app-wide (which also drops the TLS
// floor and the cleartext block on third-party cover-art fetches, the one surface we did NOT want
// to open) or leaving the URL loading system for this one origin. This is that second option.
//
// Network.framework is not subject to ATS, and `sec_protocol_options_set_verify_block` lets us
// state the trust rule we actually mean: the leaf certificate must hash to the fingerprint the
// user pinned during PIN pairing. That is a stronger check than CA trust here, not a weaker one,
// and it is the same rule the QUIC stream plane has always applied via punktfunk-core — which is
// precisely why streaming kept working over Tailscale while the library did not.

import CryptoKit
import Foundation
import Network
import Security

enum MgmtTransportError: Error, Sendable {
    /// The host's certificate did not hash to the pinned fingerprint — an impostor, or a host
    /// that was reinstalled/re-keyed since pairing.
    case pinMismatch
    case connection(String)
    case timedOut
    case tooLarge
}

enum MgmtTransport {
    /// Largest response we will buffer. The host's art proxy serves Steam hero images that run to
    /// a few MB; anything past this is not a poster and not a library payload.
    static let maxResponseBytes = 16 * 1024 * 1024

    /// `GET https://host:port/path`, authenticated by mTLS (`identity`) and pinned by
    /// `pinnedHostFingerprint` (nil = trust-on-first-use, matching the QUIC connect's semantics).
    static func get(
        host: String,
        port: UInt16,
        path: String,
        identity: SecIdentity,
        pinnedHostFingerprint: Data?,
        timeout: TimeInterval = 15
    ) async throws -> HTTPResponse {
        guard let nwPort = NWEndpoint.Port(rawValue: port) else {
            throw MgmtTransportError.connection("invalid port \(port)")
        }
        // One serial queue drives the connection, the verify block, and the timeout, so every
        // touch of `Transfer` below is already serialized — no locking, and no chance of two
        // callbacks racing to resume the continuation.
        let queue = DispatchQueue(label: "io.unom.punktfunk.mgmt-transport")
        let state = Transfer()
        let options = tlsOptions(identity: identity, pin: pinnedHostFingerprint,
                                 state: state, queue: queue)
        let connection = NWConnection(
            to: .hostPort(host: NWEndpoint.Host(unbracketed(host)), port: nwPort),
            using: NWParameters(tls: options, tcp: NWProtocolTCP.Options()))
        let request = requestBytes(host: host, port: port, path: path)

        return try await withCheckedThrowingContinuation { continuation in
            func finish(_ result: Result<HTTPResponse, Error>) {
                guard !state.finished else { return }
                state.finished = true
                connection.cancel()
                continuation.resume(with: result)
            }

            // A rejected pin surfaces as a generic handshake failure; `state.pinRejected` is how
            // we recover what actually happened so the UI can say "re-pair" instead of "offline".
            func fail(_ error: NWError) {
                finish(.failure(state.pinRejected
                    ? MgmtTransportError.pinMismatch
                    : MgmtTransportError.connection(String(describing: error))))
            }

            func receiveLoop() {
                connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
                    chunk, _, isComplete, error in
                    if let chunk, !chunk.isEmpty { state.buffer.append(chunk) }
                    if state.buffer.count > maxResponseBytes {
                        finish(.failure(MgmtTransportError.tooLarge))
                        return
                    }
                    if let error { fail(error); return }
                    guard isComplete else { receiveLoop(); return }
                    // `Connection: close` means EOF ends the response — parse what we have.
                    finish(Result { try HTTPResponseParser.parse(state.buffer) })
                }
            }

            connection.stateUpdateHandler = { newState in
                switch newState {
                case .ready:
                    connection.send(content: request, completion: .contentProcessed { error in
                        if let error { fail(error); return }
                        receiveLoop()
                    })
                case .failed(let error):
                    fail(error)
                case .cancelled:
                    // Only reachable via our own `finish`, which has already resumed; the guard
                    // in `finish` makes this a no-op rather than a double-resume crash.
                    finish(.failure(MgmtTransportError.connection("cancelled")))
                default:
                    break
                }
            }
            queue.asyncAfter(deadline: .now() + timeout) {
                finish(.failure(MgmtTransportError.timedOut))
            }
            connection.start(queue: queue)
        }
    }

    /// Mutable per-request state. Confined to the transport's serial queue — see `get`.
    private final class Transfer: @unchecked Sendable {
        var finished = false
        var pinRejected = false
        var buffer = Data()
    }

    private static func tlsOptions(
        identity: SecIdentity, pin: Data?, state: Transfer, queue: DispatchQueue
    ) -> NWProtocolTLS.Options {
        let options = NWProtocolTLS.Options()
        let sec = options.securityProtocolOptions
        sec_protocol_options_set_min_tls_protocol_version(sec, .TLSv12)
        // Our half of the mTLS handshake: the same paired identity the host authorizes the
        // read-only library routes by (mgmt/auth.rs `cert_may_access`).
        if let secIdentity = sec_identity_create(identity) {
            sec_protocol_options_set_local_identity(sec, secIdentity)
        }
        // Replaces system trust evaluation wholesale, which is the point: the host is self-signed
        // and carries no SAN, so there is nothing for the system policy to succeed at. Pinning the
        // leaf's SHA-256 is the real check.
        sec_protocol_options_set_verify_block(sec, { _, trust, complete in
            let secTrust = sec_trust_copy_ref(trust).takeRetainedValue()
            guard let chain = SecTrustCopyCertificateChain(secTrust) as? [SecCertificate],
                  let leaf = chain.first
            else {
                state.pinRejected = true
                complete(false)
                return
            }
            guard let pin else {
                complete(true) // trust-on-first-use: no pin recorded for this host yet
                return
            }
            let fingerprint = Data(SHA256.hash(data: SecCertificateCopyData(leaf) as Data))
            let matches = fingerprint == pin
            if !matches { state.pinRejected = true }
            complete(matches)
        }, queue)
        return options
    }

    private static func requestBytes(host: String, port: UInt16, path: String) -> Data {
        // An IPv6 literal is bracketed in the Host header (RFC 9110 §7.2); a name or IPv4 is not.
        let bare = unbracketed(host)
        let authority = bare.contains(":") ? "[\(bare)]:\(port)" : "\(bare):\(port)"
        let request = """
            GET \(path) HTTP/1.1\r
            Host: \(authority)\r
            User-Agent: punktfunk-apple\r
            Accept: */*\r
            Connection: close\r
            \r

            """
        return Data(request.utf8)
    }

    /// Saved hosts store bare addresses, but a user who pasted a bracketed IPv6 literal shouldn't
    /// get an unresolvable endpoint out of it.
    private static func unbracketed(_ host: String) -> String {
        guard host.hasPrefix("["), host.hasSuffix("]"), host.count > 2 else { return host }
        return String(host.dropFirst().dropLast())
    }
}
