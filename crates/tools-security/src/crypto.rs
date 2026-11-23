//! Password-based file encryption used by Security Lock.
//!
//! Format (all integers little-endian):
//! ```text
//! magic "WMRSLOCK" | version u8 = 1 | m_cost u32 | t_cost u32 | p_cost u32
//! | salt [16] | nonce_prefix [8] | chunk*
//! ```
//! The key is Argon2id(password, salt). Plaintext is split into 1 MiB chunks; chunk `i`
//! is AES-256-GCM encrypted with nonce = prefix ‖ i (u32 BE) and AAD = i (u64 LE) ‖
//! is_last (u8). The final chunk (possibly empty) is always shorter than a full chunk
//! and is the only one with is_last = 1, so truncation, reordering and appending are
//! all detected by authentication failure.

use std::io::{Read, Write};

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Nonce, Payload};
use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

pub const MAGIC: &[u8; 8] = b"WMRSLOCK";
pub const VERSION: u8 = 1;
pub const CHUNK: usize = 1024 * 1024;
const TAG: usize = 16;
const HEADER_LEN: usize = 8 + 1 + 12 + 16 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl KdfParams {
    /// 64 MiB, 3 passes: a deliberate ~0.5 s per unlock on a desktop CPU.
    pub const DEFAULT: KdfParams = KdfParams {
        m_cost_kib: 64 * 1024,
        t_cost: 3,
        p_cost: 1,
    };
    /// Cheap parameters for unit tests only.
    #[cfg(test)]
    pub const TEST: KdfParams = KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    };
}

fn derive_key(password: &str, salt: &[u8; 16], p: KdfParams) -> Result<Aes256Gcm> {
    let params = argon2::Params::new(p.m_cost_kib, p.t_cost, p.p_cost, Some(32))
        .map_err(|e| anyhow::anyhow!("invalid key derivation parameters: {e}"))?;
    let mut key = Zeroizing::new([0u8; 32]);
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|e| anyhow::anyhow!("key derivation failed: {e}"))?;
    Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| anyhow::anyhow!("invalid key length"))
}

fn nonce(prefix: &[u8; 8], index: u32) -> Nonce<Aes256Gcm> {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(prefix);
    n[8..].copy_from_slice(&index.to_be_bytes());
    Nonce::<Aes256Gcm>::try_from(&n[..]).expect("12-byte nonce")
}

fn aad(index: u32, last: bool) -> [u8; 9] {
    let mut a = [0u8; 9];
    a[..8].copy_from_slice(&u64::from(index).to_le_bytes());
    a[8] = u8::from(last);
    a
}

/// Fills `buf` as far as possible; returns the number of bytes read (< len only at EOF).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

pub fn encrypt(
    mut input: impl Read,
    mut output: impl Write,
    password: &str,
    params: KdfParams,
) -> Result<()> {
    if password.is_empty() {
        bail!("the password must not be empty");
    }
    let mut salt = [0u8; 16];
    let mut prefix = [0u8; 8];
    getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("random generator: {e}"))?;
    getrandom::fill(&mut prefix).map_err(|e| anyhow::anyhow!("random generator: {e}"))?;
    let cipher = derive_key(password, &salt, params)?;

    output.write_all(MAGIC)?;
    output.write_all(&[VERSION])?;
    for v in [params.m_cost_kib, params.t_cost, params.p_cost] {
        output.write_all(&v.to_le_bytes())?;
    }
    output.write_all(&salt)?;
    output.write_all(&prefix)?;

    let mut buf = Zeroizing::new(vec![0u8; CHUNK]);
    let mut index: u32 = 0;
    loop {
        let n = read_full(&mut input, &mut buf)?;
        let last = n < CHUNK;
        let ct = cipher
            .encrypt(
                &nonce(&prefix, index),
                Payload {
                    msg: &buf[..n],
                    aad: &aad(index, last),
                },
            )
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        output.write_all(&ct)?;
        if last {
            break;
        }
        index = index.checked_add(1).context("file too large")?;
    }
    output.flush()?;
    Ok(())
}

pub fn decrypt(mut input: impl Read, mut output: impl Write, password: &str) -> Result<()> {
    let mut header = [0u8; HEADER_LEN];
    if read_full(&mut input, &mut header)? < HEADER_LEN || &header[..8] != MAGIC {
        bail!("not a Security Lock file");
    }
    if header[8] != VERSION {
        bail!("unsupported Security Lock version {}", header[8]);
    }
    let u32_at = |o: usize| u32::from_le_bytes(header[o..o + 4].try_into().unwrap());
    let params = KdfParams {
        m_cost_kib: u32_at(9),
        t_cost: u32_at(13),
        p_cost: u32_at(17),
    };
    // Refuse absurd parameters from a tampered header (memory exhaustion).
    if params.m_cost_kib > 4 * 1024 * 1024 || params.t_cost > 64 || params.p_cost > 64 {
        bail!("invalid file header");
    }
    let salt: [u8; 16] = header[21..37].try_into().unwrap();
    let prefix: [u8; 8] = header[37..45].try_into().unwrap();
    let cipher = derive_key(password, &salt, params)?;

    let mut buf = vec![0u8; CHUNK + TAG];
    let mut index: u32 = 0;
    loop {
        let n = read_full(&mut input, &mut buf)?;
        // A full-size chunk is never the last one; anything shorter must be.
        let last = n < CHUNK + TAG;
        let pt = Zeroizing::new(
            cipher
                .decrypt(
                    &nonce(&prefix, index),
                    Payload {
                        msg: &buf[..n],
                        aad: &aad(index, last),
                    },
                )
                .map_err(|_| anyhow::anyhow!("wrong password or damaged file"))?,
        );
        output.write_all(&pt)?;
        if last {
            break;
        }
        index = index.checked_add(1).context("file too large")?;
    }
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(len: usize) {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let mut enc = Vec::new();
        encrypt(&data[..], &mut enc, "correct horse", KdfParams::TEST).unwrap();
        assert_eq!(&enc[..8], MAGIC);
        let mut dec = Vec::new();
        decrypt(&enc[..], &mut dec, "correct horse").unwrap();
        assert_eq!(dec, data, "len {len}");
    }

    #[test]
    fn roundtrips_at_chunk_boundaries() {
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK] {
            roundtrip(len);
        }
    }

    #[test]
    fn rejects_wrong_password_tampering_and_truncation() {
        let data = vec![7u8; CHUNK + 100];
        let mut enc = Vec::new();
        encrypt(&data[..], &mut enc, "pw", KdfParams::TEST).unwrap();

        assert!(
            decrypt(&enc[..], &mut Vec::new(), "PW").is_err(),
            "wrong password"
        );

        let mut flipped = enc.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 1;
        assert!(decrypt(&flipped[..], &mut Vec::new(), "pw").is_err(), "bit flip");

        // Dropping the final chunk leaves a full-size chunk marked not-last → rejected.
        let truncated = &enc[..HEADER_LEN + CHUNK + TAG];
        assert!(decrypt(truncated, &mut Vec::new(), "pw").is_err(), "truncation");

        let mut appended = enc.clone();
        appended.extend_from_slice(&[0u8; 32]);
        assert!(
            decrypt(&appended[..], &mut Vec::new(), "pw").is_err(),
            "appended data"
        );

        assert!(
            decrypt(
                &b"not a lock file at all, definitely not"[..],
                &mut Vec::new(),
                "pw"
            )
            .is_err()
        );
        assert!(
            encrypt(&data[..], &mut Vec::new(), "", KdfParams::TEST).is_err(),
            "empty password"
        );
    }

    #[test]
    fn same_input_encrypts_differently() {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        encrypt(&b"same"[..], &mut a, "pw", KdfParams::TEST).unwrap();
        encrypt(&b"same"[..], &mut b, "pw", KdfParams::TEST).unwrap();
        assert_ne!(a, b, "random salt and nonce prefix");
    }
}
