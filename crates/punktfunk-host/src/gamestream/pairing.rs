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
use std::sync::Mutex;
use std::time::Duration;
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
/// What that leaves is **capture**, and the answer to it is that a PIN is never a global slot:
/// every parked handshake is keyed by [`CeremonyId`] (`uniqueid` + client-cert fingerprint), the
/// management API reports those identities, and a submit is ADDRESSED — to the named ceremony,
/// or to the only one parked. A racer that parks after the operator submits can never consume a
/// PIN typed for someone else, and a PIN cannot outlive the ceremony it was addressed to
/// (security-review 2026-08-31 H-4; predecessors 2026-08-15 #7, 2026-08-25).
///
/// The cap below does leak one bit to an unauthenticated peer — a refused `getservercert` tells it
/// that `MAX_PARKED_WAITERS` handshakes are already parked. That is inherent to having a cap, and
/// the alternative (an unbounded pre-auth park) is the worse trade.
const MAX_PARKED_WAITERS: usize = 4;

/// Identity of one parked pairing ceremony: the wire `uniqueid` and the lowercase-hex SHA-256
/// fingerprint of the client certificate presented in phase 1. This is what the operator's
/// console shows next to the PIN prompt, and what a submit names.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CeremonyId {
    pub uniqueid: String,
    pub fingerprint: String,
}

/// What [`PinGate::submit`] did with the operator's PIN.
#[derive(Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Addressed to exactly this ceremony.
    Delivered(CeremonyId),
    /// Nothing is parked — refuse rather than hold a PIN for whoever knocks next.
    NoWaiter,
    /// The given `uniqueid`/`fingerprint` filters matched no parked ceremony.
    NoMatch,
    /// More than one parked ceremony matched — the caller must narrow the target.
    Ambiguous(Vec<CeremonyId>),
}

pub struct PinGate {
    /// Parked ceremonies → the PIN addressed to each (`None` until the operator submits). An
    /// entry exists exactly while its handshake is parked in [`take`](Self::take), so a PIN can
    /// only ever be written toward a live, named waiter and dies with it.
    waiters: Mutex<HashMap<CeremonyId, Option<String>>>,
    notify: Notify,
}

impl PinGate {
    fn new() -> Self {
        PinGate {
            waiters: Mutex::new(HashMap::new()),
            notify: Notify::new(),
        }
    }

    /// Deliver the operator's PIN to the ceremony the filters select. With no filters the PIN
    /// goes to the sole parked ceremony; with several parked, the submit must name its target —
    /// otherwise we refuse rather than hand the secret to a racer.
    pub fn submit(
        &self,
        pin: String,
        uniqueid: Option<&str>,
        fingerprint: Option<&str>,
    ) -> SubmitOutcome {
        let mut w = self.waiters.lock().unwrap();
        if w.is_empty() {
            return SubmitOutcome::NoWaiter;
        }
        let matches: Vec<CeremonyId> = w
            .keys()
            .filter(|k| uniqueid.is_none_or(|u| k.uniqueid == u))
            .filter(|k| fingerprint.is_none_or(|f| k.fingerprint.eq_ignore_ascii_case(f)))
            .cloned()
            .collect();
        match matches.len() {
            0 => SubmitOutcome::NoMatch,
            1 => {
                let id = matches.into_iter().next().expect("len checked");
                *w.get_mut(&id).expect("key from this map") = Some(pin);
                drop(w);
                self.notify.notify_waiters();
                tracing::info!(
                    uniqueid = %id.uniqueid,
                    fingerprint = %id.fingerprint,
                    "pairing: PIN addressed to its ceremony"
                );
                SubmitOutcome::Delivered(id)
            }
            _ => {
                tracing::warn!(
                    "pairing: more than one handshake matches — refusing an ambiguous submit"
                );
                SubmitOutcome::Ambiguous(matches)
            }
        }
    }

    /// True while at least one pairing handshake is parked waiting for the user's PIN.
    pub fn awaiting_pin(&self) -> bool {
        !self.waiters.lock().unwrap().is_empty()
    }

    /// The parked ceremonies, for the management API's pairing status — the console shows these
    /// identities so the operator answers a NAMED prompt rather than a bare one.
    pub fn pending(&self) -> Vec<CeremonyId> {
        let mut v: Vec<CeremonyId> = self.waiters.lock().unwrap().keys().cloned().collect();
        v.sort_by(|a, b| (&a.uniqueid, &a.fingerprint).cmp(&(&b.uniqueid, &b.fingerprint)));
        v
    }

    async fn take(&self, timeout: Duration, id: &CeremonyId) -> Option<String> {
        {
            // Bound the number of pairing handshakes parked at once: each `getservercert` is
            // pre-auth and parks for up to 300s, so without a cap an unpaired LAN peer could pin
            // unbounded tasks + keep `awaiting_pin` asserted (security-review 2026-06-28 #12).
            let mut w = self.waiters.lock().unwrap();
            if w.len() >= MAX_PARKED_WAITERS {
                tracing::warn!("pairing: too many handshakes awaiting a PIN — refusing");
                return None;
            }
            // A twin park under the SAME identity would race one addressed PIN between two
            // tasks; a real client runs one handshake at a time, so the twin is refused.
            if w.contains_key(id) {
                tracing::warn!(
                    uniqueid = %id.uniqueid,
                    "pairing: this ceremony is already awaiting a PIN — refusing a twin"
                );
                return None;
            }
            w.insert(id.clone(), None);
        }
        // Remove the entry on every exit path (PIN delivered, timeout, or future cancellation) —
        // an addressed-but-unconsumed PIN dies with its ceremony instead of waiting to
        // authenticate whoever knocks next (security-review 2026-08-25).
        struct WaiterGuard<'a> {
            gate: &'a PinGate,
            id: &'a CeremonyId,
        }
        impl Drop for WaiterGuard<'_> {
            fn drop(&mut self) {
                self.gate.waiters.lock().unwrap().remove(self.id);
            }
        }
        let _guard = WaiterGuard { gate: self, id };

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Register interest BEFORE checking the slot: `notify_waiters` only wakes futures
            // already polled/enabled, so the reverse order can lose the one wakeup a submit
            // sends and leave a delivered PIN unread until the deadline.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(pin) = self
                .waiters
                .lock()
                .unwrap()
                .get_mut(id)
                .and_then(Option::take)
            {
                return Some(pin);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
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

        // The ceremony parks under ITS identity — uniqueid plus the fingerprint of the very cert
        // phase 4 would pin — so the operator's PIN can be addressed to this handshake and no
        // other (security-review 2026-08-31 H-4).
        let ceremony = CeremonyId {
            uniqueid: uniqueid.to_string(),
            fingerprint: hex::encode(crypto::sha256(&[der.as_slice()])),
        };
        tracing::info!(
            uniqueid,
            fingerprint = %ceremony.fingerprint,
            "pairing phase 1 (getservercert) — awaiting PIN: deliver it via the management \
             API `POST /api/v1/pair/pin` (operator reads the PIN off the Moonlight client)"
        );
        let pin = self
            .pin
            .take(Duration::from_secs(300), &ceremony)
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

    fn cid(tag: &str) -> CeremonyId {
        CeremonyId {
            uniqueid: tag.to_string(),
            fingerprint: hex::encode(crypto::sha256(&[tag.as_bytes()])),
        }
    }

    /// `awaiting_pin`/`pending` flip on while `take` is parked and back off on every exit
    /// path (delivered + timeout) — the management API's pairing UX depends on it.
    #[tokio::test]
    async fn pin_gate_reports_waiting() {
        let pairing = Arc::new(Pairing::new());
        assert!(!pairing.pin.awaiting_pin());

        let waiter = {
            let p = pairing.clone();
            tokio::spawn(async move { p.pin.take(Duration::from_secs(5), &cid("dev-a")).await })
        };
        while !pairing.pin.awaiting_pin() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(pairing.pin.pending(), vec![cid("dev-a")]);

        // A bare submit reaches the sole parked ceremony, and names it in the outcome.
        assert_eq!(
            pairing.pin.submit("1234".into(), None, None),
            SubmitOutcome::Delivered(cid("dev-a"))
        );
        assert_eq!(waiter.await.unwrap().as_deref(), Some("1234"));
        assert!(!pairing.pin.awaiting_pin());

        // Timeout path also clears the entry.
        assert_eq!(
            pairing
                .pin
                .take(Duration::from_millis(10), &cid("dev-a"))
                .await,
            None
        );
        assert!(!pairing.pin.awaiting_pin());
    }

    /// With nothing parked a submit stores nothing: a PIN can never sit waiting to authenticate
    /// whoever knocks next (security-review 2026-08-25 — structural now, the slot is per-ceremony).
    #[tokio::test]
    async fn pin_without_a_waiter_is_refused_and_never_stored() {
        let pairing = Pairing::new();
        assert_eq!(
            pairing.pin.submit("1234".into(), None, None),
            SubmitOutcome::NoWaiter
        );
        // The handshake arriving right after gets no inherited PIN.
        assert_eq!(
            pairing
                .pin
                .take(Duration::from_millis(5), &cid("dev-a"))
                .await,
            None
        );
    }

    /// The PIN is ADDRESSED (security-review 2026-08-31 H-4): with two ceremonies parked, a bare
    /// submit is refused as ambiguous, a named submit reaches exactly its target, and the other
    /// waiter — a racer parked at the right moment — times out with nothing.
    #[tokio::test]
    async fn pin_is_delivered_only_to_the_named_ceremony() {
        let pairing = Arc::new(Pairing::new());
        let legit = {
            let p = pairing.clone();
            tokio::spawn(async move { p.pin.take(Duration::from_secs(5), &cid("legit")).await })
        };
        let racer = {
            let p = pairing.clone();
            tokio::spawn(async move { p.pin.take(Duration::from_millis(200), &cid("racer")).await })
        };
        while pairing.pin.pending().len() < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Ambiguous without a name; nothing is delivered.
        assert!(matches!(
            pairing.pin.submit("1234".into(), None, None),
            SubmitOutcome::Ambiguous(ids) if ids.len() == 2
        ));
        // A name that matches nothing delivers nothing.
        assert_eq!(
            pairing.pin.submit("1234".into(), Some("nobody"), None),
            SubmitOutcome::NoMatch
        );
        // Named: only the legit ceremony receives; the racer starves.
        assert_eq!(
            pairing.pin.submit("1234".into(), Some("legit"), None),
            SubmitOutcome::Delivered(cid("legit"))
        );
        assert_eq!(legit.await.unwrap().as_deref(), Some("1234"));
        assert_eq!(racer.await.unwrap(), None);
    }

    /// Two parked ceremonies sharing a `uniqueid` (an attacker mimicking the legit device's id)
    /// stay distinguishable by certificate fingerprint — the id alone is ambiguous, the
    /// fingerprint resolves it.
    #[tokio::test]
    async fn shared_uniqueid_is_resolved_by_fingerprint() {
        let pairing = Arc::new(Pairing::new());
        let a = CeremonyId {
            uniqueid: "dev".into(),
            fingerprint: "aa".repeat(32),
        };
        let b = CeremonyId {
            uniqueid: "dev".into(),
            fingerprint: "bb".repeat(32),
        };
        let (ka, kb) = (a.clone(), b.clone());
        let wa = {
            let p = pairing.clone();
            tokio::spawn(async move { p.pin.take(Duration::from_secs(5), &ka).await })
        };
        let wb = {
            let p = pairing.clone();
            tokio::spawn(async move { p.pin.take(Duration::from_millis(200), &kb).await })
        };
        while pairing.pin.pending().len() < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(matches!(
            pairing.pin.submit("1234".into(), Some("dev"), None),
            SubmitOutcome::Ambiguous(_)
        ));
        assert_eq!(
            pairing
                .pin
                .submit("1234".into(), Some("dev"), Some(&a.fingerprint)),
            SubmitOutcome::Delivered(a)
        );
        assert_eq!(wa.await.unwrap().as_deref(), Some("1234"));
        assert_eq!(wb.await.unwrap(), None);
    }

    /// A pre-auth peer flood can park at most `MAX_PARKED_WAITERS` pairing handshakes; the next
    /// `take` is refused immediately (returns `None` without parking), bounding the 300s-waiter DoS
    /// (security-review 2026-06-28 #12). A twin park under one identity is refused too.
    #[tokio::test]
    async fn pin_gate_caps_parked_waiters() {
        let pairing = Arc::new(Pairing::new());
        let mut handles = Vec::new();
        for i in 0..MAX_PARKED_WAITERS {
            let p = pairing.clone();
            handles.push(tokio::spawn(async move {
                p.pin
                    .take(Duration::from_secs(5), &cid(&format!("dev-{i}")))
                    .await
            }));
        }
        // Wait until all the slots are taken.
        while pairing.pin.pending().len() < MAX_PARKED_WAITERS {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // One more is refused right away (no parking), even with a long timeout.
        assert_eq!(
            pairing
                .pin
                .take(Duration::from_secs(5), &cid("extra"))
                .await,
            None
        );
        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn twin_park_under_one_identity_is_refused() {
        let pairing = Arc::new(Pairing::new());
        let first = {
            let p = pairing.clone();
            tokio::spawn(async move { p.pin.take(Duration::from_millis(300), &cid("dev")).await })
        };
        while !pairing.pin.awaiting_pin() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(
            pairing.pin.take(Duration::from_secs(5), &cid("dev")).await,
            None
        );
        first.abort();
    }
}
