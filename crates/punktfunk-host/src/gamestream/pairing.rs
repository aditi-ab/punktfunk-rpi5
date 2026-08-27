//! The 4-phase GameStream pairing state machine (over HTTP), keyed by `uniqueid`. Proves
//! both sides know the PIN (via the SHA-256(salt||pin) AES-ECB key) and own their certs
//! (RSA signatures), then pins the client cert. The final `pairchallenge` happens over
//! HTTPS (handled in `nvhttp`). Byte-exact spec: `design/research/…-research.json`.

use super::cert::ServerIdentity;
use super::crypto;
use anyhow::{anyhow, bail, Context, Result};
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::RsaPublicKey;
// `rsa`'s own re-export — `VerifyingKey<Sha256>` below is an `rsa 0.9` / `digest 0.10` type
// parameter, distinct from the crate-wide `sha2 0.11`. See Cargo.toml.
use rsa::sha2::Sha256;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Out-of-band PIN delivery. Moonlight generates + displays a PIN; the operator submits it
/// via the bearer-authenticated management API (`POST /api/v1/pair/pin`) only — there is no
/// unauthenticated nvhttp delivery path (a network client must never be able to submit its
/// own PIN; security-review 2026-06-28 #1). `getservercert` parks until a PIN arrives.
/// Max pairing handshakes parked in [`PinGate::take`] at once (each holds a slot for up to
/// 300s), bounding a pre-auth waiter flood. Real pairing is one operator-driven client at a time.
///
/// **On brute-forcing the 4-digit PIN** (audited 2026-08-27, apollo-comparison #96): 10⁴ is a
/// small space, but nothing here is guessing at it, because **a network peer has no way to submit
/// a PIN**. Submission is `POST /api/v1/pair/pin` on the bearer-authenticated management API and
/// nowhere else, so there is no oracle to hammer and no attempt counter worth adding — a
/// per-attempt cap would bound the *operator's* typos, not an attacker. A wrong PIN fails the
/// ceremony and costs the attacker a fresh client handshake *and* a fresh operator submission,
/// which is not a loop anyone can automate from the network.
///
/// What that leaves is not brute force but **capture**: the PIN slot is global and bound to no
/// particular handshake, so a peer parked at the right moment can take the PIN the operator typed
/// for someone else. That is the real residual, it is already narrowed twice — [`PinGate::submit`]
/// refuses while more than one handshake is parked, and an unconsumed PIN expires rather than
/// waiting to authenticate whoever knocks next — and the full fix is to key the gate by
/// `uniqueid` (which also needs the management API to say *which* device is asking, so the
/// operator answers a named prompt rather than a bare one).
///
/// The cap below does leak one bit to an unauthenticated peer — a refused `getservercert` tells it
/// that `MAX_PARKED_WAITERS` handshakes are already parked. That is inherent to having a cap, and
/// the alternative (an unbounded pre-auth park) is the worse trade.
const MAX_PARKED_WAITERS: usize = 4;

pub struct PinGate {
    /// The submitted PIN and when it was submitted — a PIN no handshake consumed within the
    /// pairing window is discarded rather than held for the next one ([`PinGate::take`]).
    pin: Mutex<Option<(String, Instant)>>,
    notify: Notify,
    /// Handshakes currently parked in [`take`](Self::take) — drives the management API's
    /// `pin_pending` so a control pane knows when to prompt for the PIN.
    waiters: AtomicUsize,
}

impl PinGate {
    fn new() -> Self {
        PinGate {
            pin: Mutex::new(None),
            notify: Notify::new(),
            waiters: AtomicUsize::new(0),
        }
    }

    /// Deliver the operator's PIN to a parked handshake. Returns `false` (delivering nothing) when
    /// more than one handshake is parked.
    ///
    /// The PIN is a single global slot with no binding to a specific handshake, so with N parked
    /// waiters whichever polls first takes it. An attacker who floods `getservercert` slots (up to
    /// `MAX_PARKED_WAITERS - 1`) while the operator is pairing could therefore take the operator's
    /// PIN, derive the ceremony key from its own salt, and pin its own certificate. Real pairing is
    /// one operator-driven client at a time, so when the target is ambiguous we refuse rather than
    /// hand the secret to a racer — the operator retries once the flood clears. This narrows the
    /// window to a tight post-submit timing race; a full fix keys the gate by `uniqueid` (see the
    /// design note). security-review 2026-08-15 finding 7.
    pub fn submit(&self, pin: String) -> bool {
        if self.waiters.load(Ordering::SeqCst) > 1 {
            tracing::warn!(
                "pairing: more than one handshake is awaiting a PIN — refusing an ambiguous submit"
            );
            return false;
        }
        *self.pin.lock().unwrap() = Some((pin, Instant::now()));
        self.notify.notify_waiters();
        true
    }

    /// True while a pairing handshake is parked waiting for the user's PIN.
    pub fn awaiting_pin(&self) -> bool {
        self.waiters.load(Ordering::SeqCst) > 0
    }

    async fn take(&self, timeout: Duration) -> Option<String> {
        // Bound the number of pairing handshakes parked at once: each `getservercert` is
        // pre-auth and parks for up to 300s, so without a cap an unpaired LAN peer could pin
        // unbounded tasks + keep `awaiting_pin` asserted (security-review 2026-06-28 #12).
        // Reserve a slot atomically; refuse (treated as "no PIN") once the cap is reached.
        if self
            .waiters
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_PARKED_WAITERS).then_some(n + 1)
            })
            .is_err()
        {
            tracing::warn!("pairing: too many handshakes awaiting a PIN — refusing");
            return None;
        }
        // Decrement on every exit path (PIN delivered, timeout, or future cancellation).
        struct WaiterGuard<'a>(&'a AtomicUsize);
        impl Drop for WaiterGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _guard = WaiterGuard(&self.waiters);

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // A PIN must not outlive the handshake it was typed for: the slot is global and
            // unbound, so a PIN submitted with no handshake left to consume it (the submitter's
            // waiter died between the management API's `awaiting_pin` check and the submit) used
            // to sit here indefinitely and authenticate whoever knocked next — a PIN typed for
            // one device pairing another (security-review 2026-08-25). Its shelf life is the same
            // pairing window a handshake parks for, and a real client is ALREADY parked when the
            // operator submits, so it takes the PIN within microseconds — never near this bound.
            if let Some((p, at)) = self.pin.lock().unwrap().take() {
                if at.elapsed() < timeout {
                    return Some(p);
                }
                tracing::warn!("pairing: discarding a PIN no handshake consumed in time");
            }
            if tokio::time::timeout_at(deadline, self.notify.notified())
                .await
                .is_err()
            {
                return None;
            }
        }
    }
}

/// Per-client pairing session carried across the 4 separate HTTP GETs.
struct Session {
    aes_key: [u8; 16],
    client_cert_der: Vec<u8>,
    client_cert_sig: Vec<u8>,
    client_pubkey: RsaPublicKey,
    serversecret: [u8; 16],
    server_challenge: [u8; 16],
    /// The client's phase-3 hash, recomputed + checked in phase 4.
    client_hash: Vec<u8>,
    /// Set once phase 3 has produced the RSA-signed serversecret. A repeated phase 3 is refused so a
    /// peer past phase 1 can't loop phase2/phase3 to harvest many signing-time samples (a passive
    /// timing-oracle amplifier vs. the rsa-crate Marvin side-channel; see `.cargo/audit.toml`).
    responded: bool,
}

pub struct Pairing {
    sessions: Mutex<HashMap<String, Session>>,
    pub pin: PinGate,
}

impl Pairing {
    pub fn new() -> Self {
        Pairing {
            sessions: Mutex::new(HashMap::new()),
            pin: PinGate::new(),
        }
    }

    /// Phase 1: store the client cert, await the PIN, derive the AES key, return our cert.
    pub async fn getservercert(
        &self,
        id: &ServerIdentity,
        uniqueid: &str,
        salt_hex: &str,
        clientcert_hex: &str,
    ) -> Result<String> {
        let salt_bytes = hex::decode(salt_hex).context("salt hex")?;
        if salt_bytes.len() < 16 {
            bail!("salt too short");
        }
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&salt_bytes[..16]);
        let pem_bytes = hex::decode(clientcert_hex).context("clientcert hex")?;
        let (der, sig, pubkey) = parse_client_cert(&pem_bytes)?;

        tracing::info!(
            uniqueid,
            "pairing phase 1 (getservercert) — awaiting PIN: deliver it via the management \
             API `POST /api/v1/pair/pin` (operator reads the PIN off the Moonlight client)"
        );
        let pin = self
            .pin
            .take(Duration::from_secs(300))
            .await
            .ok_or_else(|| anyhow!("no PIN submitted within 300s"))?;
        let aes_key = crypto::pin_key(&salt, &pin);

        self.sessions.lock().unwrap().insert(
            uniqueid.to_string(),
            Session {
                aes_key,
                client_cert_der: der,
                client_cert_sig: sig,
                client_pubkey: pubkey,
                serversecret: [0; 16],
                server_challenge: [0; 16],
                client_hash: Vec::new(),
                responded: false,
            },
        );
        tracing::info!(
            uniqueid,
            "pairing phase 1 — PIN accepted, returning host cert"
        );
        let inner = format!(
            "<plaincert>{}</plaincert>",
            hex::encode(id.cert_pem.as_bytes())
        );
        Ok(paired_xml(&inner, true))
    }

    /// Phase 2: decrypt the client challenge, return our hash + server challenge.
    pub fn clientchallenge(
        &self,
        id: &ServerIdentity,
        uniqueid: &str,
        hexv: &str,
    ) -> Result<String> {
        let mut map = self.sessions.lock().unwrap();
        let s = map
            .get_mut(uniqueid)
            .ok_or_else(|| anyhow!("no pairing session"))?;
        let enc = hex::decode(hexv).context("clientchallenge hex")?;
        let client_challenge = crypto::ecb_decrypt(&s.aes_key, &enc);
        if client_challenge.len() < 16 {
            bail!("short client challenge");
        }
        s.serversecret = crypto::random();
        s.server_challenge = crypto::random();
        let server_hash =
            crypto::sha256(&[&client_challenge[..16], &id.signature, &s.serversecret]);
        let mut plain = Vec::with_capacity(48);
        plain.extend_from_slice(&server_hash);
        plain.extend_from_slice(&s.server_challenge);
        let resp = crypto::ecb_encrypt(&s.aes_key, &plain);
        let inner = format!(
            "<challengeresponse>{}</challengeresponse>",
            hex::encode(resp)
        );
        Ok(paired_xml(&inner, true))
    }

    /// Phase 3: store the client's hash, return our RSA-signed serversecret.
    pub fn serverchallengeresp(
        &self,
        id: &ServerIdentity,
        uniqueid: &str,
        hexv: &str,
    ) -> Result<String> {
        let mut map = self.sessions.lock().unwrap();
        let s = map
            .get_mut(uniqueid)
            .ok_or_else(|| anyhow!("no pairing session"))?;
        let enc = hex::decode(hexv).context("serverchallengeresp hex")?;
        let client_hash = crypto::ecb_decrypt(&s.aes_key, &enc);
        if client_hash.len() < 32 {
            bail!("short challenge response");
        }
        s.client_hash = client_hash[..32].to_vec();
        // Sign the serversecret exactly ONCE per ceremony: refuse a repeated phase 3 so a peer that
        // cleared phase 1 (operator PIN) can't replay it to collect many RSA signing-time samples
        // (timing-oracle amplifier vs. RUSTSEC-2023-0071; see `.cargo/audit.toml`). A legit client
        // signs once. The session stays for phase 4 (the cert-pin step) but won't re-sign.
        if s.responded {
            bail!("serverchallengeresp already answered for this pairing session");
        }
        s.responded = true;
        let sig: Signature = id.signing_key.sign(&s.serversecret);
        let mut secret = Vec::with_capacity(16 + 256);
        secret.extend_from_slice(&s.serversecret);
        secret.extend_from_slice(&sig.to_vec());
        let inner = format!("<pairingsecret>{}</pairingsecret>", hex::encode(secret));
        Ok(paired_xml(&inner, true))
    }

    /// Phase 4: verify the client knew the PIN (hash match) and owns its cert (RSA verify);
    /// on success, pin the client cert.
    pub fn clientpairingsecret(
        &self,
        uniqueid: &str,
        hexv: &str,
        paired_store: &Mutex<Vec<Vec<u8>>>,
    ) -> Result<String> {
        let mut map = self.sessions.lock().unwrap();
        let s = map
            .get_mut(uniqueid)
            .ok_or_else(|| anyhow!("no pairing session"))?;
        let data = hex::decode(hexv).context("clientpairingsecret hex")?;
        if data.len() < 16 {
            bail!("short pairing secret");
        }
        let client_secret = &data[..16];
        let client_sig = &data[16..];
        let expected = crypto::sha256(&[&s.server_challenge, &s.client_cert_sig, client_secret]);
        // Constant-time compare so a timing side-channel can't probe the expected hash.
        let hash_ok = crypto::ct_eq(&expected, &s.client_hash);
        let sig_ok = verify256(&s.client_pubkey, client_secret, client_sig).is_ok();
        // Clone what the success branch needs so the `&mut map` borrow (`s`) is released before we
        // mutate the map below.
        let client_cert_der = s.client_cert_der.clone();
        // The pairing session is single-use: remove it now, WHATEVER the outcome. Phase 4 runs over
        // plain HTTP, so a passive observer captures the request; without this, a replay re-passes the
        // hash/sig check and re-pins the (same) cert over and over — unbounded `paired`/paired.json
        // growth + PairingCompleted event spam until restart (security-review 2026-07-17).
        map.remove(uniqueid);
        if hash_ok && sig_ok {
            {
                let mut store = paired_store.lock().unwrap();
                // De-dup: re-pairing an already-trusted cert must not append a duplicate DER.
                if !store.iter().any(|der| der == &client_cert_der) {
                    store.push(client_cert_der.clone());
                    super::save_paired(&store);
                }
            }
            tracing::info!(uniqueid, "pairing phase 4 complete — client cert pinned");
            // Lifecycle event, plane parity with `NativePairing::add` (RFC §4). GameStream
            // pairing has no device name — the client's uniqueid is the identity it presents.
            crate::events::emit(crate::events::EventKind::PairingCompleted {
                device: crate::events::DeviceRef {
                    name: uniqueid.to_string(),
                    fingerprint: hex::encode(crypto::sha256(&[client_cert_der.as_slice()])),
                    plane: crate::events::Plane::Gamestream,
                },
            });
            Ok(paired_xml("", true))
        } else {
            tracing::warn!(
                uniqueid,
                hash_ok,
                sig_ok,
                "pairing phase 4 rejected — PIN or cert mismatch"
            );
            Ok(paired_xml("", false))
        }
    }
}

fn verify256(pubkey: &RsaPublicKey, msg: &[u8], sig: &[u8]) -> Result<()> {
    let vk = VerifyingKey::<Sha256>::new(pubkey.clone());
    let signature = Signature::try_from(sig).context("parse client signature")?;
    vk.verify(msg, &signature)
        .context("verify client signature")?;
    Ok(())
}

fn parse_client_cert(pem_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>, RsaPublicKey)> {
    let (_, pem) =
        x509_parser::pem::parse_x509_pem(pem_bytes).map_err(|e| anyhow!("client cert pem: {e}"))?;
    let der = pem.contents.clone();
    let x509 = pem.parse_x509().context("parse client x509")?;
    let sig = x509.signature_value.data.to_vec();
    let pubkey =
        RsaPublicKey::from_public_key_der(x509.public_key().raw).context("client rsa pubkey")?;
    Ok((der, sig, pubkey))
}

/// `<root status_code="200"><paired>0|1</paired> inner </root>`.
fn paired_xml(inner: &str, paired: bool) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">\n<paired>{}</paired>\n{}</root>\n",
        u8::from(paired),
        inner
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// `awaiting_pin` flips true while `take` is parked and back to false on every exit
    /// path (delivered + timeout) — the management API's pairing UX depends on it.
    #[tokio::test]
    async fn pin_gate_reports_waiting() {
        let pairing = Arc::new(Pairing::new());
        assert!(!pairing.pin.awaiting_pin());

        let waiter = {
            let p = pairing.clone();
            tokio::spawn(async move { p.pin.take(Duration::from_secs(5)).await })
        };
        while !pairing.pin.awaiting_pin() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        pairing.pin.submit("1234".into());
        assert_eq!(waiter.await.unwrap().as_deref(), Some("1234"));
        assert!(!pairing.pin.awaiting_pin());

        // Timeout path also clears the flag.
        assert_eq!(pairing.pin.take(Duration::from_millis(10)).await, None);
        assert!(!pairing.pin.awaiting_pin());
    }

    /// A PIN nobody consumed must NOT authenticate the next handshake to arrive: the slot is
    /// global and unbound, so a PIN typed for a device that gave up would otherwise pair whoever
    /// knocked afterwards (security-review 2026-08-25).
    #[tokio::test]
    async fn unconsumed_pin_is_discarded() {
        let pairing = Pairing::new();
        pairing.pin.submit("1234".into()); // nothing parked — nobody takes it
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Older than the window this handshake parks for: discarded, not handed over…
        assert_eq!(pairing.pin.take(Duration::from_millis(5)).await, None);
        // …and gone for good, so the handshake after it doesn't inherit it either.
        assert_eq!(pairing.pin.take(Duration::from_millis(5)).await, None);
    }

    /// A pre-auth peer flood can park at most `MAX_PARKED_WAITERS` pairing handshakes; the next
    /// `take` is refused immediately (returns `None` without parking), bounding the 300s-waiter DoS
    /// (security-review 2026-06-28 #12).
    #[tokio::test]
    async fn pin_gate_caps_parked_waiters() {
        let pairing = Arc::new(Pairing::new());
        let mut handles = Vec::new();
        for _ in 0..MAX_PARKED_WAITERS {
            let p = pairing.clone();
            handles.push(tokio::spawn(async move {
                p.pin.take(Duration::from_secs(5)).await
            }));
        }
        // Wait until all the slots are taken.
        while pairing.pin.waiters.load(Ordering::SeqCst) < MAX_PARKED_WAITERS {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // One more is refused right away (no parking), even with a long timeout.
        assert_eq!(pairing.pin.take(Duration::from_secs(5)).await, None);
        for h in handles {
            h.abort();
        }
    }
}
