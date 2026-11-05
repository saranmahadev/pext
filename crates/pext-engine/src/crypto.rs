//! Passphrase-based authenticated encryption for backups and sync snapshots.
//!
//! Format (`.pextbak`):
//!   magic "PEXTBAK1" | m_kib u32 | t u32 | p u32 | salt 16 | nonce 24 | ciphertext+tag
//! The whole header is bound to the ciphertext as associated data, so any edit
//! to it (or to the body) fails authentication. Key = Argon2id(passphrase, salt).
//! Cipher = XChaCha20-Poly1305.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use pext_core::{Error, Result};

const MAGIC: &[u8; 8] = b"PEXTBAK1";
const HEADER: usize = 8 + 12 + 16 + 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        KdfParams {
            m_kib: 64 * 1024,
            t: 3,
            p: 1,
        }
    }
}

impl KdfParams {
    /// Deliberately weak parameters for tests only.
    pub fn insecure_fast() -> Self {
        KdfParams {
            m_kib: 256,
            t: 1,
            p: 1,
        }
    }
}

/// A secret that never prints itself.
#[derive(Clone)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| {
        Error::internal(
            "no_randomness",
            format!("cannot read system randomness: {e}"),
        )
    })?;
    Ok(b)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn derive(pass: &str, salt: &[u8], k: KdfParams) -> Result<[u8; 32]> {
    // Bounds protect against a crafted file asking for absurd work.
    if k.m_kib < 8 || k.m_kib > 1024 * 1024 || k.t == 0 || k.t > 20 || k.p == 0 || k.p > 16 {
        return Err(Error::validation(
            "bad_kdf_params",
            "the file asks for unreasonable key-derivation settings",
        ));
    }
    let params = Params::new(k.m_kib, k.t, k.p, Some(32))
        .map_err(|e| Error::internal("kdf_failed", e.to_string()))?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(pass.as_bytes(), salt, &mut out)
        .map_err(|e| Error::internal("kdf_failed", e.to_string()))?;
    Ok(out)
}

pub fn check_passphrase(pass: &str) -> Result<()> {
    if pass.chars().count() < 8 {
        return Err(Error::validation(
            "weak_passphrase",
            "the passphrase must be at least 8 characters",
        )
        .with_hint("use a long phrase; it protects every backup and sync snapshot"));
    }
    Ok(())
}

pub fn encrypt(plain: &[u8], pass: &str, k: KdfParams) -> Result<Vec<u8>> {
    check_passphrase(pass)?;
    let salt: [u8; 16] = random_bytes()?;
    let nonce: [u8; 24] = random_bytes()?;
    let key = derive(pass, &salt, k)?;
    let mut out = Vec::with_capacity(HEADER + plain.len() + 16);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&k.m_kib.to_le_bytes());
    out.extend_from_slice(&k.t.to_le_bytes());
    out.extend_from_slice(&k.p.to_le_bytes());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| Error::internal("cipher_init", "bad key length"))?;
    let ct = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plain,
                aad: &out,
            },
        )
        .map_err(|_| Error::internal("encrypt_failed", "encryption failed"))?;
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn decrypt(blob: &[u8], pass: &str) -> Result<Vec<u8>> {
    if blob.len() < HEADER + 16 || &blob[..8] != MAGIC {
        return Err(
            Error::validation("not_a_backup", "this is not a pext encrypted file")
                .with_hint("was it truncated or created by another tool?"),
        );
    }
    let u32at = |o: usize| u32::from_le_bytes([blob[o], blob[o + 1], blob[o + 2], blob[o + 3]]);
    let k = KdfParams {
        m_kib: u32at(8),
        t: u32at(12),
        p: u32at(16),
    };
    let salt = &blob[20..36];
    let nonce: [u8; 24] = blob[36..60].try_into().expect("length checked");
    let key = derive(pass, salt, k)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| Error::internal("cipher_init", "bad key length"))?;
    cipher
        .decrypt(
            (&nonce).into(),
            Payload {
                msg: &blob[HEADER..],
                aad: &blob[..HEADER],
            },
        )
        .map_err(|_| {
            Error::auth(
                "decrypt_failed",
                "wrong passphrase, or the file is damaged or has been modified",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper_detection() {
        let k = KdfParams::insecure_fast();
        let blob = encrypt(b"hello ledger", "correct horse battery", k).unwrap();
        assert_eq!(
            decrypt(&blob, "correct horse battery").unwrap(),
            b"hello ledger"
        );
        assert_eq!(
            decrypt(&blob, "wrong passphrase!").unwrap_err().code,
            "decrypt_failed"
        );
        // flipping any single byte anywhere must be detected
        for i in (0..blob.len()).step_by(7) {
            let mut b = blob.clone();
            b[i] ^= 1;
            assert!(decrypt(&b, "correct horse battery").is_err(), "byte {i}");
        }
        assert_eq!(
            decrypt(&blob[..20], "correct horse battery")
                .unwrap_err()
                .code,
            "not_a_backup"
        );
        assert_eq!(decrypt(b"", "x").unwrap_err().code, "not_a_backup");
    }

    #[test]
    fn same_input_encrypts_differently_each_time() {
        let k = KdfParams::insecure_fast();
        let a = encrypt(b"x", "passphrase-1", k).unwrap();
        let b = encrypt(b"x", "passphrase-1", k).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn weak_passphrases_and_hostile_params_are_refused() {
        assert_eq!(
            encrypt(b"x", "short", KdfParams::insecure_fast())
                .unwrap_err()
                .code,
            "weak_passphrase"
        );
        let mut blob = encrypt(b"x", "passphrase-1", KdfParams::insecure_fast()).unwrap();
        blob[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decrypt(&blob, "passphrase-1").unwrap_err().code,
            "bad_kdf_params"
        );
    }

    #[test]
    fn secret_does_not_leak_in_debug() {
        assert_eq!(
            format!("{:?}", Secret("hunter2hunter2".into())),
            "Secret(***)"
        );
    }
}
