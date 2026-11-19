//! Link tokens, ids and device secrets.

/// Hex digits in a link token (256 bits).
pub const TOKEN_HEX: usize = 64;
/// Hex digits in share, comment and event ids (64 bits; unique, not secret).
pub const ID_HEX: usize = 16;
/// Hex digits of a device secret the page sends (128 bits or more).
pub const DEVICE_HEX: usize = 32;

/// `n` random bytes as lowercase hex, from the operating system's generator.
pub fn random_hex(n: usize) -> std::io::Result<String> {
    let bytes = random_bytes(n)?;
    Ok(hex(&bytes))
}

#[cfg(unix)]
fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

#[cfg(not(unix))]
fn random_bytes(_n: usize) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "review links need a Unix random source"))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new link token.
pub fn new_token() -> std::io::Result<String> {
    random_hex(TOKEN_HEX / 2)
}

/// A new share, comment or event id.
pub fn new_id() -> std::io::Result<String> {
    random_hex(ID_HEX / 2)
}

/// Compare two secrets without stopping at the first difference.
pub fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether `s` has the shape of a link token (so nothing else is looked up).
pub fn is_token(s: &str) -> bool {
    crate::text::is_hex(s, TOKEN_HEX)
}

/// What is stored for a device secret: its SHA-256 in hex. `None` for a secret of the wrong shape.
#[cfg(feature = "server")]
pub fn device_hash(secret: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let ok = (DEVICE_HEX..=128).contains(&secret.len()) && secret.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then(|| hex(&Sha256::digest(secret.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_and_well_formed() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert!(is_token(&a) && is_token(&b));
        assert_ne!(a, b);
        assert!(crate::text::is_hex(&new_id().unwrap(), ID_HEX));
    }

    #[test]
    fn secrets_compare_exactly() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "abcd"));
        assert!(!same_secret("", "a"));
    }

    #[cfg(feature = "server")]
    #[test]
    fn device_secrets_are_hashed_and_checked() {
        let s = "0123456789abcdef0123456789abcdef";
        let h = device_hash(s).unwrap();
        assert_eq!(h.len(), 64);
        assert_ne!(h, s);
        assert!(device_hash("short").is_none());
        assert!(device_hash(&"z".repeat(40)).is_none());
        assert!(device_hash(&"a".repeat(129)).is_none());
    }
}
