//! Password hashing with Argon2id (feature `password`).
//!
//! Hashes are PHC strings (`$argon2id$v=19$m=19456,t=2,p=1$...`) that carry
//! their salt and parameters, so stored hashes keep verifying if the
//! defaults change. Hashing is deliberately slow (tens of milliseconds), so
//! the async functions run it on the blocking thread pool instead of
//! stalling the server's event loop.
//!
//! ```
//! # async fn demo() -> zapline::Result<()> {
//! use zapline::auth::password;
//!
//! let hash = password::hash("correct horse battery staple").await?;
//! assert!(password::verify("correct horse battery staple", &hash).await?);
//! assert!(!password::verify("wrong", &hash).await?);
//! # Ok(()) }
//! ```
//!
//! On login, call [`verify_missing`] when the account does not exist: it
//! takes as long as a real check, so response times don't reveal which
//! emails are registered.
//!
//! For password-reset and email-confirmation links, [`new_token`] makes a
//! random token to send and [`hash_token`] the digest to store, so a leaked
//! database doesn't hand out working links.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

use crate::{Error, Result};

/// Hash `password` (Argon2id, OWASP-recommended parameters, random salt).
pub async fn hash(password: impl Into<String>) -> Result<String> {
    let password = password.into();
    blocking(move || hash_blocking(&password)).await
}

/// Whether `password` matches `hash` (a PHC string from [`hash`]). A
/// malformed hash is an error, not a mismatch.
pub async fn verify(password: impl Into<String>, hash: impl Into<String>) -> Result<bool> {
    let (password, hash) = (password.into(), hash.into());
    blocking(move || verify_blocking(&password, &hash)).await
}

/// Spend the time of a [`verify`] for an account that doesn't exist, then
/// return `false`.
pub async fn verify_missing(password: impl Into<String>) -> Result<bool> {
    let password = password.into();
    blocking(move || {
        static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let dummy = match DUMMY.get() {
            Some(h) => h.clone(),
            None => {
                let h = hash_blocking("zapline-dummy-password")?;
                DUMMY.get_or_init(|| h).clone()
            }
        };
        verify_blocking(&password, &dummy).map(|_| false)
    })
    .await
}

/// [`hash`] on the current thread.
pub fn hash_blocking(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).map_err(|e| {
        Error::internal("secure random numbers are unavailable")
            .with_source(std::io::Error::other(e.to_string()))
    })?;
    let salt = SaltString::encode_b64(&salt).map_err(hash_error)?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(hash_error)
}

/// [`verify`] on the current thread.
pub fn verify_blocking(password: &str, hash: &str) -> Result<bool> {
    let parsed = PasswordHash::new(hash).map_err(hash_error)?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(hash_error(e)),
    }
}

/// A random single-use token for a link (256 bits, base64url, 43
/// characters). Send it; store [`hash_token`] of it.
///
/// ```
/// use zapline::auth::password::{hash_token, new_token};
///
/// let token = new_token()?;           // goes in the emailed link
/// let stored = hash_token(&token);    // goes in the database
/// assert_eq!(stored, hash_token(&token));
/// assert_ne!(token, new_token()?);
/// # Ok::<(), zapline::Error>(())
/// ```
///
/// # Errors
///
/// If the operating system's random number generator is unavailable.
pub fn new_token() -> Result<String> {
    use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| {
        Error::internal("secure random numbers are unavailable")
            .with_source(std::io::Error::other(e.to_string()))
    })?;
    Ok(BASE64_URL_SAFE_NO_PAD.encode(buf))
}

/// The SHA-256 digest of `token` as lowercase hex: what to store and look
/// up for a token from [`new_token`]. (Tokens are random, so a fast hash
/// is enough; Argon2 is for passwords people choose.)
pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
        hex.push(char::from(b"0123456789abcdef"[usize::from(b & 15)]));
    }
    hex
}

fn hash_error(e: argon2::password_hash::Error) -> Error {
    Error::internal("password hashing failed").with_source(std::io::Error::other(e.to_string()))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    match tokio::runtime::Handle::try_current() {
        Ok(_) => tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| Error::internal("password hashing task failed").with_source(e))?,
        // Outside a runtime (a plain #[test]): just run it.
        Err(_) => f(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_verify_and_salts_differ() {
        let a = hash_blocking("hunter2").unwrap();
        let b = hash_blocking("hunter2").unwrap();
        assert!(a.starts_with("$argon2id$"));
        assert_ne!(a, b);
        assert!(verify_blocking("hunter2", &a).unwrap());
        assert!(!verify_blocking("hunter3", &a).unwrap());
        assert!(verify_blocking("hunter2", "not a hash").is_err());
    }
}
