//! One-time recovery codes, for users who lose their authenticator (feature
//! `two-factor`).
//!
//! Show the codes once when two-factor login is turned on, store only their
//! hashes, and remove a hash when its code is used.
//!
//! ```
//! use zapline::auth::recovery_codes;
//!
//! let codes = recovery_codes::generate(10);        // show these to the user, once
//! let mut stored: Vec<String> =                   // keep only these
//!     codes.iter().map(|c| recovery_codes::hash(c)).collect();
//!
//! // At login, instead of a TOTP code:
//! let typed = codes[3].to_uppercase();            // users retype them loosely
//! let used = recovery_codes::find(&typed, &stored).expect("a valid code");
//! stored.remove(used);                            // each code works once
//! assert!(recovery_codes::find(&typed, &stored).is_none());
//! ```

use sha2::{Digest, Sha256};

use super::totp::constant_time_eq;

/// Letters and digits that can't be confused when read aloud or retyped
/// (no 0/o, 1/l/i): 31 of them.
const ALPHABET: &[u8; 31] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// A uniformly random character of [`ALPHABET`]: bytes of 248 or more are
/// redrawn, so no character is likelier than another.
fn random_char() -> char {
    loop {
        let mut b = [0u8; 1];
        if getrandom::getrandom(&mut b).is_err() {
            b[0] = fastrand::u8(..);
        }
        if b[0] < 248 {
            return ALPHABET[usize::from(b[0] % 31)] as char;
        }
    }
}

/// `n` new codes like `x7k2m-q9pd4`: 10 characters (about 49 bits) from an
/// unambiguous alphabet, in two groups.
pub fn generate(n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            let chars: String = (0..10).map(|_| random_char()).collect();
            format!("{}-{}", &chars[..5], &chars[5..])
        })
        .collect()
}

/// What users type, compared loosely: case, spaces and dashes don't matter.
fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The hash to store for `code` (SHA-256, hex). Codes are random and long
/// enough that a fast hash is safe; no salt is needed.
pub fn hash(code: &str) -> String {
    Sha256::digest(normalize(code).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The index in `hashes` of the hash `code` matches, to remove it; `None`
/// when it matches none.
pub fn find(code: &str, hashes: &[String]) -> Option<usize> {
    if normalize(code).len() != 10 {
        return None;
    }
    let candidate = hash(code);
    let mut found = None;
    for (i, h) in hashes.iter().enumerate() {
        if constant_time_eq(candidate.as_bytes(), h.as_bytes()) && found.is_none() {
            found = Some(i);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_readable_and_loosely_matched() {
        let codes = generate(50);
        let mut unique = codes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 50);
        for c in &codes {
            assert_eq!(c.len(), 11, "{c}");
            assert!(!c.contains(['0', 'o', '1', 'l', 'i']), "{c}");
        }
        let hashes: Vec<String> = codes.iter().map(|c| hash(c)).collect();
        let loose = codes[7].replace('-', " ").to_uppercase();
        assert_eq!(find(&loose, &hashes), Some(7));
        assert_eq!(find("aaaaa-aaaaa", &hashes), None);
        assert_eq!(find("", &hashes), None);
    }
}
