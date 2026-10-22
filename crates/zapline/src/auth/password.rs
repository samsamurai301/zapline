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
