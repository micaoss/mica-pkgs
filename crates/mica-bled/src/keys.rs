//! The protocol's derivations (sections 4 and 5): SPAKE2's w, the session
//! keys and the sealed message stream.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use p256::Scalar;
use p256::elliptic_curve::ff::FromUniformBytes;
use sha2::{Digest, Sha256, Sha512};

use crate::spake2::write_length_prefixed;

/// The protocol version spoken.
pub const VERSION: u64 = 1;
/// SPAKE2's associated data.
pub const AAD: &[u8] = b"mica-ble/1";

/// SPAKE2's w for a device and a secret.
pub fn password_scalar(device_id: &str, secret: &[u8]) -> Scalar {
    let mut input = Vec::new();
    write_length_prefixed(&mut input, b"mica-ble/1 w");
    write_length_prefixed(&mut input, device_id.as_bytes());
    write_length_prefixed(&mut input, secret);
    let hash: [u8; 64] = Sha512::digest(&input).into();
    Scalar::from_uniform_bytes(&hash)
}

/// The two directional keys derived from Ke.
pub struct SessionKeys {
    pub phone_to_device: [u8; 32],
    pub device_to_phone: [u8; 32],
}

/// Derive the session keys from SPAKE2's Ke.
pub fn session_keys(ke: &[u8]) -> SessionKeys {
    let hkdf = Hkdf::<Sha256>::new(None, ke);
    let mut keys = SessionKeys {
        phone_to_device: [0; 32],
        device_to_phone: [0; 32],
    };
    // 32 bytes is far below HKDF-SHA256's limit, so neither expand fails.
    let _ = hkdf.expand(b"mica-ble/1 phone->device", &mut keys.phone_to_device);
    let _ = hkdf.expand(b"mica-ble/1 device->phone", &mut keys.device_to_phone);
    keys
}

/// A sealed message that does not open.
#[derive(Debug, PartialEq, Eq)]
pub struct Unsealed;

impl std::fmt::Display for Unsealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("sealed message does not authenticate")
    }
}

impl std::error::Error for Unsealed {}

/// One direction of the session: ChaCha20-Poly1305, nonce `0^4 || counter`.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(&(*key).into()),
            counter: 0,
        }
    }

    fn next_nonce(&mut self) -> Nonce {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        nonce.into()
    }

    /// Seal the next message.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Unsealed> {
        let nonce = self.next_nonce();
        self.cipher.encrypt(&nonce, plaintext).map_err(|_| Unsealed)
    }

    /// Open the next message.
    pub fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>, Unsealed> {
        let nonce = self.next_nonce();
        self.cipher.decrypt(&nonce, sealed).map_err(|_| Unsealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_opens_once_and_in_order() {
        let key = [9; 32];
        let (mut tx, mut rx) = (Sealer::new(&key), Sealer::new(&key));
        let first = tx.seal(b"one").unwrap();
        let second = tx.seal(b"two").unwrap();
        assert_ne!(first[..3], second[..3]);
        assert_eq!(rx.open(&first).unwrap(), b"one");
        assert_eq!(rx.open(&first), Err(Unsealed));
        let mut rx = Sealer::new(&key);
        assert_eq!(rx.open(&second), Err(Unsealed));
    }

    #[test]
    fn a_tampered_message_does_not_open() {
        let key = [3; 32];
        let mut sealed = Sealer::new(&key).seal(b"payload").unwrap();
        sealed[0] ^= 1;
        assert_eq!(Sealer::new(&key).open(&sealed), Err(Unsealed));
        assert_eq!(Sealer::new(&key).open(&[0; 15]), Err(Unsealed));
    }
}
