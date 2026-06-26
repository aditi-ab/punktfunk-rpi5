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
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;

/// Out-of-band PIN delivery. Moonlight generates + displays a PIN; the user submits it
/// (via the management API's `POST /api/v1/pair/pin` or nvhttp's `GET /pin?pin=NNNN`).
/// `getservercert` parks until a PIN arrives.
pub struct PinGate {
    pin: Mutex<Option<String>>,
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

    pub fn submit(&self, pin: String) {
        *self.pin.lock().unwrap() = Some(pin);
        self.notify.notify_waiters();
    }

    /// True while a pairing handshake is parked waiting for the user's PIN.
    pub fn awaiting_pin(&self) -> bool {
        self.waiters.load(Ordering::SeqCst) > 0
    }

    async fn take(&self, timeout: Duration) -> Option<String> {
        self.waiters.fetch_add(1, Ordering::SeqCst);
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
            if let Some(p) = self.pin.lock().unwrap().take() {
                return Some(p);
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
            "pairing phase 1 (getservercert) — awaiting PIN: submit `GET /pin?pin=NNNN`"
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
        if hash_ok && sig_ok {
            {
                let mut store = paired_store.lock().unwrap();
                store.push(s.client_cert_der.clone());
                super::save_paired(&store);
            }
            tracing::info!(uniqueid, "pairing phase 4 — SUCCESS, client cert pinned");
            Ok(paired_xml("", true))
        } else {
            tracing::warn!(
                uniqueid,
                hash_ok,
                sig_ok,
                "pairing phase 4 — FAILED (PIN/cert)"
            );
            map.remove(uniqueid);
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
}
