//! Host SPAKE2 pairing for native QUIC sessions.
//!
//! `serve_session` dispatches a first-message `PairRequest` here after resolving
//! the armed PIN. This is SPAKE2 role B: the PIN is consumed after the host
//! confirmation so an attacker gets one online guess, then the client fingerprint
//! is persisted on success.
//!
//! A lapsed or re-armed window mints nothing (`consume_window`; tests at the
//! foot). Protocol: `punktfunk_core::quic::pake`. Access stored via
//! `crate::native_pairing`.

use super::*;
use crate::native_pairing::sanitize_device_name;
use punktfunk_core::quic::{PairChallenge, PairProof, PairResult};

/// 60 s: a person reads the PIN off the host and types it. Session handshake is machine-speed.
const PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Host SPAKE2 (role B). Consumes the armed PIN after the challenge write so a
/// vanished client still burns the one online guess. Both stream writes time out.
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
    // Unpaired wire name: scrub once here and log only that value.
    // ANSI/C0 and bidi otherwise reach the operator terminal and the journal.
    let name = sanitize_device_name(&req.name, &client_fp_hex);

    tracing::info!(
        name = %name,
        client = %client_fp_hex,
        "PAIRING REQUEST — verifying against the armed PIN"
    );

    // `false` = SPAKE2 role B. Identities are the cert we presented and the cert we received.
    let (pake, spake_b) = pake::start(false, pin, &client_fp, host_fp);
    let confirms = pake.finish(&req.spake_a)?; // Err only on a malformed peer message

    // Timeout: this write waits on the client's stream window. Sequential host;
    // the armed TTL can lapse while parked.
    tokio::time::timeout(
        PAIRING_TIMEOUT,
        io::write_msg(
            &mut send,
            &PairChallenge {
                spake_b,
                confirm: confirms.host,
            }
            .encode(),
        ),
    )
    .await
    .map_err(|_| anyhow!("pairing timed out sending the challenge"))??;

    // Consume now — the challenge lets the client test this guess; a wrong PIN
    // disconnects without a proof, so there is no host-visible miss to wait for.
    // Malformed `finish` never reached here, so garbage does not burn the window.
    let access = consume_window(np, pin)?;

    let proof = tokio::time::timeout(PAIRING_TIMEOUT, io::read_msg(&mut recv))
        .await
        .map_err(|_| anyhow!("pairing timed out waiting for the client's confirmation"))??;
    let proof = PairProof::decode(&proof).map_err(|e| anyhow!("PairProof decode: {e:?}"))?;

    // Wrong PIN or split certs: different SPAKE2 key; MAC mismatch. No offline search.
    let ok = pake::verify(&confirms.client, &proof.confirm);

    if ok {
        if let Err(e) = np.add_with_access(&req.name, &fingerprint_hex(&client_fp), access) {
            tracing::error!(error = %format!("{e:#}"), "could not persist paired clients");
        }
        tracing::info!(name = %name, "pairing complete — client trusted");
    } else {
        tracing::warn!(name = %name, "pairing rejected (wrong PIN) — fingerprint not stored");
    }
    // Same flow-control trap as the challenge write.
    tokio::time::timeout(
        PAIRING_TIMEOUT,
        io::write_msg(&mut send, &PairResult { ok }.encode()),
    )
    .await
    .map_err(|_| anyhow!("pairing timed out sending the result"))??;
    let _ = send.finish();
    // 5 s: wait for the client to ACK PairResult before we close. A vanished
    // peer must not occupy the sequential host.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.closed()).await;
    conn.close(0u32.into(), b"pairing done");
    anyhow::ensure!(ok, "pairing rejected (wrong PIN)");
    Ok(())
}

/// Snapshot access, then prove this is still the PIN we started with, then disarm.
/// Reverse order fails open: [`NativePairing::add_with_access`] treats a missing
/// choice as full/permanent. `Err` if the window lapsed or was re-armed — leave
/// the live window untouched so a stalling client cannot wipe the next one.
fn consume_window(np: &NativePairing, pin: &str) -> Result<Option<crate::native_pairing::Access>> {
    let access = np.armed_access();
    anyhow::ensure!(
        np.current_pin().as_deref() == Some(pin),
        "the pairing window lapsed while the ceremony was running"
    );
    np.disarm();
    Ok(access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_pairing::Access;
    use std::time::Duration;

    fn temp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pf-native-ceremony-{tag}-{}.json",
            std::process::id()
        ))
    }

    fn controller_4h() -> Access {
        Access {
            grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
            expires_unix: Some(4 * 3600),
        }
    }

    /// Lapsed window mints nothing. Without the PIN re-check, expiry reads as
    /// no choice and `add_with_access(.., None)` is full/permanent.
    #[test]
    fn expired_window_mints_no_grant() {
        let p = temp("expired");
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();

        let pin = np.arm_for(Duration::from_secs(60), None, Some(controller_4h()));
        assert_eq!(consume_window(&np, &pin).unwrap(), Some(controller_4h()));
        assert!(np.current_pin().is_none(), "single-use: window consumed");

        // Duration::ZERO is already past when read: refuse, do not mint full/permanent.
        let pin = np.arm_for(Duration::ZERO, None, Some(controller_4h()));
        assert!(consume_window(&np, &pin).is_err());

        // Stale PIN after re-arm: refuse and do not disarm the live window.
        let stale = np.arm_for(Duration::ZERO, None, Some(controller_4h()));
        // 4-digit PIN: 1 in 10_000 collision; re-arm until the values differ.
        let mut fresh = np.arm_for(Duration::from_secs(60), None, Some(controller_4h()));
        while fresh == stale {
            fresh = np.arm_for(Duration::from_secs(60), None, Some(controller_4h()));
        }
        assert!(consume_window(&np, &stale).is_err());
        assert_eq!(np.current_pin().as_deref(), Some(fresh.as_str()));

        let _ = std::fs::remove_file(&p);
    }

    /// CLI `--allow-pairing` has no choice and no expiry: `None` is the legitimate
    /// full/permanent default, not a lapse, and must still pair.
    #[test]
    fn choiceless_window_still_pairs() {
        let p = temp("choiceless");
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), Some("4321".into()), true).unwrap();
        assert_eq!(consume_window(&np, "4321").unwrap(), None);
        let _ = std::fs::remove_file(&p);
    }
}
