//! Identifiers: schema names to TypeScript types, operation ids to methods,
//! parameter names to arguments.

use serde_json::Value;

/// Lowercase words of an identifier-ish string (`userProfile`,
/// `user_profile`, `HTTPServer` -> `http server`).
fn words(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if c.is_ascii_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            let boundary = prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_is_lower);
            if boundary {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// `get_one_get` -> `getOneGet`.
pub(crate) fn camel_case(s: &str) -> String {
    let mut out = String::new();
    for (i, w) in words(s).iter().enumerate() {
        if i == 0 {
            out.push_str(w);
        } else {
            out.push_str(&capitalize(w));
        }
    }
    out
}

/// Names the generated code declares itself, or global TypeScript/DOM types
/// a schema must not shadow.
const RESERVED_TYPES: &[&str] = &[
    "ApiError",
    "Array",
    "Blob",
    "Boolean",
    "BufferSource",
    "CallOptions",
    "Client",
    "ClientOptions",
    "Date",
    "Event",
    "FieldError",
    "File",
    "FormData",
    "FormValue",
    "Headers",
    "Map",
    "Number",
    "Object",
    "Omit",
    "Partial",
    "Pick",
    "Promise",
    "QueryValue",
    "Record",
    "Request",
    "Response",
    "Set",
    "String",
    "Symbol",
    "URL",
    "ValidationError",
    "ValidationFields",
];

/// The TypeScript name of schema `name`. zapline's `Error` body becomes
/// `ErrorBody` so the global `Error` class stays usable.
pub(crate) fn type_name(name: &str) -> String {
    let n: String = words(name).iter().map(|w| capitalize(w)).collect();
    if n.is_empty() || n.starts_with(|c: char| c.is_ascii_digit()) {
        format!("T{n}")
    } else if n == "Error" {
        "ErrorBody".into()
    } else if RESERVED_TYPES.contains(&n.as_str()) {
        format!("{n}Schema")
    } else {
        n
    }
}

/// The method name for an operation id.
pub(crate) fn method_name(id: &str) -> String {
    let name = camel_case(id);
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("op{}", capitalize(&name))
    } else {
        name
    }
}

/// A valid JS identifier for a parameter.
pub(crate) fn js_ident(s: &str) -> String {
    let id = camel_case(s);
    match id.as_str() {
        "" => "param".into(),
        "arguments" | "await" | "break" | "case" | "catch" | "class" | "const" | "continue"
        | "debugger" | "default" | "delete" | "do" | "else" | "enum" | "eval" | "export"
        | "extends" | "false" | "finally" | "for" | "function" | "if" | "implements" | "import"
        | "in" | "instanceof" | "interface" | "let" | "new" | "null" | "package" | "private"
        | "protected" | "public" | "return" | "static" | "super" | "switch" | "this" | "throw"
        | "true" | "try" | "typeof" | "var" | "void" | "while" | "with" | "yield" => {
            format!("{id}_")
        }
        _ if id.starts_with(|c: char| c.is_ascii_digit()) => format!("p{id}"),
        _ => id,
    }
}

/// Whether `name` can be an unquoted object key.
pub(crate) fn is_ident(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// An object key: bare when it is an identifier, quoted otherwise.
pub(crate) fn prop_name(name: &str) -> String {
    if is_ident(name) {
        name.to_string()
    } else {
        Value::from(name).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(camel_case("get_one_get"), "getOneGet");
        assert_eq!(camel_case("users_list"), "usersList");
        assert_eq!(camel_case("allArgs"), "allArgs");
        assert_eq!(camel_case("HTTPServer"), "httpServer");
        assert_eq!(method_name("2fa"), "op2fa");
        assert_eq!(js_ident("user_id"), "userId");
        assert_eq!(js_ident("default"), "default_");
        assert_eq!(type_name("Page_User"), "PageUser");
        assert_eq!(type_name("Error"), "ErrorBody");
        assert_eq!(type_name("Response"), "ResponseSchema");
        assert_eq!(type_name("ValidationError"), "ValidationErrorSchema");
        assert_eq!(prop_name("a-b"), "\"a-b\"");
        assert_eq!(prop_name("ok_1"), "ok_1");
    }
}
