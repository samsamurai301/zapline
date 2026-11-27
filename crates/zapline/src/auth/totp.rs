//! Time-based one-time passwords (RFC 6238) for two-factor login: the
//! six-digit codes authenticator apps show (feature `two-factor`).
//!
//! Enrollment: make a secret with [`Totp::generate`], show
//! [`otpauth_uri`](Totp::otpauth_uri) as a QR code, ask for one code to
//! confirm, then store [`secret_base32`](Totp::secret_base32) with the user
//! (encrypted, like any credential). Login: after the password, ask for a
//! code and check it with [`verify`](Totp::verify), passing the time step of
//! the user's last accepted code so a code can't be used twice.
//!
//! ```
//! use zapline::auth::totp::Totp;
//!
//! let totp = Totp::generate();
//! let uri = totp.otpauth_uri("Shop", "ada@example.com");
//! assert!(uri.starts_with("otpauth://totp/Shop:ada%40example.com?secret="));
//!
//! // Later, at login (the code the user typed; `last` from your database):
//! let code = totp.code_now();
//! let last: Option<u64> = None;
//! let step = totp.verify(&code, last).expect("a fresh code is accepted");
//! // Store `step` as the user's last step: the same code is now refused.
//! assert!(totp.verify(&code, Some(step)).is_none());
//! ```

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::{Error, Result};

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A user's TOTP secret, with the settings every authenticator app uses:
/// HMAC-SHA1, 6 digits, a 30-second step, and one step of clock drift
/// allowed either way.
#[derive(Clone)]
pub struct Totp {
    secret: Vec<u8>,
}

impl fmt::Debug for Totp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Totp").field("secret", &"[hidden]").finish()
    }
}

impl Totp {
    /// A new random 160-bit secret (as RFC 4226 recommends).
    ///
    /// ```
    /// use zapline::auth::totp::Totp;
    /// assert_eq!(Totp::generate().secret_base32().len(), 32);
    /// ```
    pub fn generate() -> Self {
        let mut secret = vec![0u8; 20];
        if getrandom::getrandom(&mut secret).is_err() {
            for chunk in secret.chunks_mut(4) {
                let r = fastrand::u32(..).to_le_bytes();
                chunk.copy_from_slice(&r[..chunk.len()]);
            }
        }
        Totp { secret }
    }

    /// The secret stored earlier with [`secret_base32`](Self::secret_base32)
    /// (spaces, dashes and lowercase are tolerated, as users copy them).
    ///
    /// # Errors
    ///
    /// When `secret` isn't base32 or is shorter than 80 bits.
    ///
    /// ```
    /// use zapline::auth::totp::Totp;
    /// let totp = Totp::from_base32("JBSW Y3DP EHPK 3PXP")?;
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn from_base32(secret: &str) -> Result<Self> {
        let mut bits: u64 = 0;
        let mut count = 0;
        let mut out = Vec::new();
        for c in secret.bytes().filter(|c| !matches!(c, b' ' | b'-' | b'=')) {
            let v = ALPHABET
                .iter()
                .position(|&a| a == c.to_ascii_uppercase())
                .ok_or_else(|| Error::bad_request("a TOTP secret is base32 (A-Z, 2-7)"))?;
            bits = (bits << 5) | v as u64;
            count += 5;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
                bits &= (1 << count) - 1;
            }
        }
        if out.len() < 10 {
            return Err(Error::bad_request("a TOTP secret has at least 80 bits"));
        }
        Ok(Totp { secret: out })
    }

    /// The secret in base32, to store with the user and to type into an app
    /// that can't scan a QR code.
    pub fn secret_base32(&self) -> String {
        let mut out = String::new();
        let mut bits: u64 = 0;
        let mut count = 0;
        for &b in &self.secret {
            bits = (bits << 8) | u64::from(b);
            count += 8;
            while count >= 5 {
                count -= 5;
                out.push(ALPHABET[((bits >> count) & 31) as usize] as char);
            }
        }
        if count > 0 {
            out.push(ALPHABET[((bits << (5 - count)) & 31) as usize] as char);
        }
        out
    }

    /// The `otpauth://` URI an authenticator app reads from a QR code;
    /// `issuer` is your app's name and `account` the user's login.
    pub fn otpauth_uri(&self, issuer: &str, account: &str) -> String {
        // Everything but RFC 3986's unreserved characters.
        const LABEL: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
            .remove(b'.')
            .remove(b'-')
            .remove(b'_')
            .remove(b'~');
        let enc = |s: &str| percent_encoding::utf8_percent_encode(s, LABEL).to_string();
        format!(
            "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
            enc(issuer),
            enc(account),
            self.secret_base32(),
            enc(issuer),
        )
    }

    /// The code for time step `step` (seconds since 1970 divided by 30).
    fn code_for(&self, step: u64) -> u32 {
        let mut mac =
            Hmac::<Sha1>::new_from_slice(&self.secret).expect("HMAC takes any key length");
        mac.update(&step.to_be_bytes());
        let hash = mac.finalize().into_bytes();
        let offset = (hash[19] & 0x0f) as usize;
        let bin = u32::from_be_bytes([
            hash[offset] & 0x7f,
            hash[offset + 1],
            hash[offset + 2],
            hash[offset + 3],
        ]);
        bin % 1_000_000
    }

    /// The six-digit code for `unix_seconds`.
    ///
    /// ```
    /// use zapline::auth::totp::Totp;
    /// // RFC 6238's test secret, at 1970-01-01 00:00:59.
    /// let totp = Totp::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")?;
    /// assert_eq!(totp.code_at(59), "287082");
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn code_at(&self, unix_seconds: u64) -> String {
        format!("{:06}", self.code_for(unix_seconds / 30))
    }

    /// The code an app shows right now (for tests and demos).
    pub fn code_now(&self) -> String {
        self.code_at(now())
    }

    /// Check `code` against the current time step and one step either side.
    /// `last_step` is the step of this user's last accepted code (`None`
    /// before the first): codes from that step or earlier are refused, so an
    /// intercepted code can't be replayed. Returns the matched step to
    /// store, or `None`.
    pub fn verify(&self, code: &str, last_step: Option<u64>) -> Option<u64> {
        self.verify_at(code, now(), last_step)
    }

    /// [`verify`](Self::verify) at `unix_seconds`.
    ///
    /// ```
    /// use zapline::auth::totp::Totp;
    /// let totp = Totp::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")?;
    /// assert_eq!(totp.verify_at("287082", 59, None), Some(1));
    /// // 30 s later the same code is still accepted (clock drift)...
    /// assert_eq!(totp.verify_at("287082", 89, None), Some(1));
    /// // ...but not twice, and not 90 s later.
    /// assert_eq!(totp.verify_at("287082", 89, Some(1)), None);
    /// assert_eq!(totp.verify_at("287082", 150, None), None);
    /// # Ok::<(), zapline::Error>(())
    /// ```
    pub fn verify_at(&self, code: &str, unix_seconds: u64, last_step: Option<u64>) -> Option<u64> {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let now = unix_seconds / 30;
        let mut found = None;
        // Check every candidate, so timing doesn't reveal which matched.
        for step in [now.saturating_sub(1), now, now + 1] {
            let expected = format!("{:06}", self.code_for(step));
            if constant_time_eq(expected.as_bytes(), code.as_bytes())
                && last_step.is_none_or(|last| step > last)
                && found.is_none()
            {
                found = Some(step);
            }
        }
        found
    }
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B, SHA-1 column (8-digit codes there; the last six
    /// digits are what a 6-digit app shows).
    #[test]
    fn rfc_6238_vectors() {
        let totp = Totp {
            secret: b"12345678901234567890".to_vec(),
        };
        for (t, eight) in [
            (59, "94287082"),
            (1_111_111_109, "07081804"),
            (1_111_111_111, "14050471"),
            (1_234_567_890, "89005924"),
            (2_000_000_000, "69279037"),
            (20_000_000_000, "65353130"),
        ] {
            assert_eq!(totp.code_at(t), eight[2..], "t = {t}");
        }
    }

    #[test]
    fn base32_round_trips() {
        let totp = Totp::generate();
        let again = Totp::from_base32(&totp.secret_base32().to_lowercase()).unwrap();
        assert_eq!(again.secret, totp.secret);
        assert_eq!(
            Totp::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")
                .unwrap()
                .secret,
            b"12345678901234567890"
        );
        assert!(Totp::from_base32("not!base32").is_err());
        assert!(Totp::from_base32("AAAA").is_err(), "too short");
    }

    #[test]
    fn bad_input_is_refused() {
        let totp = Totp::generate();
        for code in ["", "12345", "1234567", "abcdef"] {
            assert_eq!(totp.verify(code, None), None, "{code}");
        }
        let code = totp.code_now();
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert!(totp.verify(&spaced, None).is_some());
        assert!(!format!("{totp:?}").contains(&totp.secret_base32()));
    }
}
