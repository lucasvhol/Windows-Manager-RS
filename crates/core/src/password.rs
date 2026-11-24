//! Optional program password: stored only as an Argon2id hash with a random salt.

use anyhow::{Result, bail};

const M_COST_KIB: u32 = 19 * 1024;
const T_COST: u32 = 2;

fn derive(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let params = argon2::Params::new(M_COST_KIB, T_COST, 1, Some(32)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut out = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `argon2id$<salt hex>$<hash hex>`
pub fn hash(password: &str) -> Result<String> {
    if password.is_empty() {
        bail!("the password must not be empty");
    }
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("random generator: {e}"))?;
    Ok(format!(
        "argon2id${}${}",
        hex(&salt),
        hex(&derive(password, &salt)?)
    ))
}

pub fn verify(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some("argon2id"), Some(salt), Some(expected), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let (Some(salt), Some(expected)) = (unhex(salt), unhex(expected)) else {
        return false;
    };
    let Ok(actual) = derive(password, &salt) else {
        return false;
    };
    // Constant-time comparison.
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let stored = hash("open sesame").unwrap();
        assert!(stored.starts_with("argon2id$"));
        assert!(verify("open sesame", &stored));
        assert!(!verify("open sesame!", &stored));
        assert!(!verify("open sesame", "garbage"));
        assert_ne!(hash("x").unwrap(), hash("x").unwrap(), "salted");
        assert!(hash("").is_err());
    }
}
