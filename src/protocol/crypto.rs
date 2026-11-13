//! RemSound's audio encryption: AES-256-GCM, keyed by PBKDF2-HMAC-SHA256 over the shared
//! password. Salts and the iteration count are fixed by RemSound and every other port.
//!
//! A sealed payload is `nonce (12) | tag (16) | ciphertext`.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use sha2::Sha256;

const KEY_SALT: &[u8] = b"RemSound.v1.audio-key";
const FINGERPRINT_SALT: &[u8] = b"RemSound.v1.fingerprint";
const ITERATIONS: u32 = 100_000;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
pub const OVERHEAD: usize = NONCE_BYTES + TAG_BYTES;

/// The key and fingerprint for one password. Deriving them takes a noticeable moment
/// (2 × 100,000 rounds), so do it once per password and off the GUI thread.
#[derive(Clone)]
pub struct Keys {
    pub key: [u8; 32],
    pub fingerprint: [u8; 8],
}

impl Keys {
    pub fn derive(password: &str) -> Self {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), KEY_SALT, ITERATIONS, &mut key);
        let mut fingerprint = [0u8; 8];
        pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), FINGERPRINT_SALT, ITERATIONS, &mut fingerprint);
        Self { key, fingerprint }
    }
}

/// One cipher per sending stream. Nonces are a random 48-bit prefix and a 48-bit counter, so
/// they never repeat within a session.
pub struct Sealer {
    gcm: Aes256Gcm,
    prefix: [u8; 6],
    counter: u64,
}

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Self {
        let mut prefix = [0u8; 6];
        rand::fill(&mut prefix);
        Self { gcm: Aes256Gcm::new(key.into()), prefix, counter: 0 }
    }

    /// Appends `nonce | tag | ciphertext` of `plaintext` to `out`.
    pub fn seal(&mut self, plaintext: &[u8], out: &mut Vec<u8>) {
        let mut nonce = [0u8; NONCE_BYTES];
        nonce[..6].copy_from_slice(&self.prefix);
        nonce[6..].copy_from_slice(&self.counter.to_le_bytes()[..6]);
        self.counter = self.counter.wrapping_add(1);
        let sealed = self
            .gcm
            .encrypt(&Nonce::from(nonce), Payload { msg: plaintext, aad: &[] })
            .expect("AES-GCM sealing of an in-memory buffer cannot fail");
        // The crate returns ciphertext then tag; the wire wants the tag first.
        let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_BYTES);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(tag);
        out.extend_from_slice(ciphertext);
    }
}

/// Opens sealed payloads. Stateless, so one can serve every incoming stream.
pub struct Opener {
    gcm: Aes256Gcm,
}

impl Opener {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { gcm: Aes256Gcm::new(key.into()) }
    }

    /// `None` when the payload is too short or its tag fails (another password, or tampering).
    pub fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < OVERHEAD {
            return None;
        }
        let nonce: [u8; NONCE_BYTES] = sealed[..NONCE_BYTES].try_into().ok()?;
        let mut combined = Vec::with_capacity(sealed.len() - NONCE_BYTES);
        combined.extend_from_slice(&sealed[OVERHEAD..]);
        combined.extend_from_slice(&sealed[NONCE_BYTES..OVERHEAD]);
        self.gcm.decrypt(&Nonce::from(nonce), Payload { msg: &combined, aad: &[] }).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Published cross-implementation vectors (RemSound, the remremote Mac app and engine).
    #[test]
    fn derivation_matches_other_implementations() {
        let keys = Keys::derive("test123");
        assert_eq!(hex(&keys.key), "b419d5d5ab025172af8ea4f8923ef9176bf2c0e720e40873d82aa4350f0e87d3");
        assert_eq!(hex(&keys.fingerprint), "fb6a9f52926ac190");
    }

    #[test]
    fn seal_and_open() {
        let keys = Keys::derive("pw");
        let mut sealer = Sealer::new(&keys.key);
        let opener = Opener::new(&keys.key);
        let mut sealed = Vec::new();
        sealer.seal(b"hello", &mut sealed);
        assert_eq!(sealed.len(), 5 + OVERHEAD);
        assert_eq!(opener.open(&sealed).as_deref(), Some(&b"hello"[..]));
        let mut tampered = sealed.clone();
        tampered[OVERHEAD] ^= 1;
        assert!(opener.open(&tampered).is_none());
        assert!(Opener::new(&Keys::derive("other").key).open(&sealed).is_none());
    }
}
