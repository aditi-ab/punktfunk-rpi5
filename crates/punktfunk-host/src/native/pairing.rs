//! The host side of the native SPAKE2 pairing ceremony (plan §W1 — carved out of the [`super`]
//! module). `serve_session` dispatches a connection whose first message is a `PairRequest` here,
//! after it has resolved the live arming PIN (honoring fingerprint binding, #9); this runs the
//! ceremony, enforces the single online guess, and persists the client's fingerprint on success.

use super::*;
// The ceremony-only wire messages: imported directly (native.rs no longer references them, so they
// were dropped from its `use` and won't come through `use super::*`). `PairRequest` still arrives
// via the glob (serve_session decodes it).
use crate::native_pairing::sanitize_device_name;
use punktfunk_core::quic::{PairChallenge, PairProof, PairResult};

/// Pairing needs a human in the loop (reading the PIN off the host, typing it into the
/// client), so its budget is far larger than the machine-speed session handshake.
const PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The host side of the SPAKE2 pairing ceremony (see `punktfunk_core::quic::pake`):
/// generate + display a PIN, run SPAKE2 as B binding both cert fingerprints, verify the
/// client's key-confirmation MAC (its single online guess), and persist the client's
/// fingerprint on success.
pub(super) async fn pair_ceremony(
    conn: &quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    req: PairRequest,
    host_fp: &[u8; 32],
    np: &NativePairing,
    pin: &str,
) -> Result<()> {
    use punktfunk_core::quic::pake;
    let client_fp = endpoint::peer_fingerprint(conn)
        .ok_or_else(|| anyhow!("pairing requires the client to present a certificate"))?;
    let client_fp_hex = fingerprint_hex(&client_fp);
    // Scrub the wire-supplied name ONCE, here, and log only the scrubbed value from now on.
    //
    // This name arrives from an UNPAIRED device — the earliest, least authenticated input the host
    // takes — and these were the three log sites that bypassed the documented single scrubber, so
    // ANSI/C0 escapes and bidi overrides reached the operator's terminal and the journal
    // (2026-08-05 review L-2). `sanitize_device_name` is "the one place that scrubs it" by its own
    // module doc; the storage path already went through it, only the logging did not.
    let name = sanitize_device_name(&req.name, &client_fp_hex);

    tracing::info!(
        name = %name,
        client = %client_fp_hex,
        "PAIRING REQUEST — verifying against the armed PIN"
    );

    // SPAKE2 as B; bind our own host_fp + the client cert we actually received.
    let (pake, spake_b) = pake::start(false, pin, &client_fp, host_fp);
    let confirms = pake.finish(&req.spake_a)?; // Err only on a malformed peer message

    io::write_msg(
        &mut send,
        &PairChallenge {
            spake_b,
            confirm: confirms.host,
        }
        .encode(),
    )
    .await?;

    // SINGLE-USE PIN: we've now sent the host key-confirmation, which lets the client TEST this one
    // guess (a right PIN → its proof will match; a wrong PIN → the client detects the mismatch and
    // aborts *without* sending its proof). So consume the PIN HERE — before reading the proof —
    // regardless of the outcome: an attacker gets EXACTLY ONE online guess (the documented guarantee),
    // not an unbounded brute-force of the 4-digit space against a static, never-rotating PIN. A
    // malformed request that errored at `pake.finish` above never reached here, so it doesn't burn the
    // window (no DoS from garbage). The operator re-arms (web console / restart) for the next device —
    // including after a successful pair; the protocol gives no reliable host-observable "wrong PIN"
    // signal to scope this to failures only (the client just disconnects).
    np.disarm();

    let proof = tokio::time::timeout(PAIRING_TIMEOUT, io::read_msg(&mut recv))
        .await
        .map_err(|_| anyhow!("pairing timed out waiting for the client's confirmation"))??;
    let proof = PairProof::decode(&proof).map_err(|e| anyhow!("PairProof decode: {e:?}"))?;

    // A wrong PIN (or a MITM with mismatched cert views) yields a different SPAKE2 key, so
    // the client's confirmation MAC won't match ours — one online attempt, no offline search.
    let ok = pake::verify(&confirms.client, &proof.confirm);

    if ok {
        if let Err(e) = np.add(&req.name, &fingerprint_hex(&client_fp)) {
            tracing::error!(error = %format!("{e:#}"), "could not persist paired clients");
        }
        tracing::info!(name = %name, "pairing complete — client trusted");
    } else {
        tracing::warn!(name = %name, "pairing rejected (wrong PIN) — fingerprint not stored");
    }
    io::write_msg(&mut send, &PairResult { ok }.encode()).await?;
    let _ = send.finish();
    // Wait for the client to acknowledge by closing, so the PairResult isn't dropped by our
    // close on a slow link (bounded so a vanished client can't wedge the sequential host).
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.closed()).await;
    conn.close(0u32.into(), b"pairing done");
    anyhow::ensure!(ok, "pairing rejected (wrong PIN)");
    Ok(())
}
