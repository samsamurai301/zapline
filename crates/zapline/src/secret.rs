//! Names of fields and columns whose values are never shown back.

/// Whether `name` looks like it holds a secret: it contains `password`,
/// `secret` or `token`, or ends with `_hash` (case-insensitive). The admin
/// panel hides such columns, and `Views::render_invalid` never echoes such
/// form fields back into the page.
pub(crate) fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["password", "secret", "token"]
        .iter()
        .any(|s| lower.contains(s))
        || lower.ends_with("_hash")
}

#[cfg(test)]
mod tests {
    use super::is_secret_name;

    #[test]
    fn secret_names() {
        for name in [
            "password",
            "Password_Confirm",
            "api_token",
            "client_secret",
            "pin_hash",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in ["email", "title", "hash_tags", "username"] {
            assert!(!is_secret_name(name), "{name}");
        }
    }
}
