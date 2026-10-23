//! What the dev proxy adds to pages: one `<script>` tag that loads the
//! live reload client, and the client itself.

/// Where the proxy serves the live reload client.
pub const CLIENT_PATH: &str = "/__zapline/client.js";
/// Where the proxy serves its server-sent events.
pub const EVENTS_PATH: &str = "/__zapline/events";
/// Paths under this prefix belong to the proxy and never reach the app.
pub const PREFIX: &str = "/__zapline/";

/// The browser side: live reload, CSS hot swap, the build error overlay and
/// the rebuild badge.
pub const CLIENT_JS: &str = include_str!("client.js");

/// The tag that loads the client. An external script (not inline) so the
/// usual `script-src 'self'` policy allows it; `nonce` is copied from the
/// page's Content-Security-Policy when it uses nonces.
///
/// ```
/// use zapline_cli::dev::inject::script_tag;
/// assert_eq!(
///     script_tag(None),
///     r#"<script src="/__zapline/client.js" defer></script>"#
/// );
/// ```
pub fn script_tag(nonce: Option<&str>) -> String {
    match nonce {
        Some(n) => format!(
            r#"<script src="{CLIENT_PATH}" defer nonce="{}"></script>"#,
            attr(n)
        ),
        None => format!(r#"<script src="{CLIENT_PATH}" defer></script>"#),
    }
}

/// `html` with the client tag added: before `</head>`, else right after the
/// opening `<body>` tag, else before `</body>` or `</html>`. Fragments (no
/// `<html>`, `<head>`, `<body>` or doctype, e.g. an htmx partial) and pages
/// that already load the client are returned unchanged.
///
/// ```
/// use zapline_cli::dev::inject::inject;
/// let page = inject("<html><head><title>x</title></head><body>hi</body></html>", None);
/// assert_eq!(
///     page,
///     r#"<html><head><title>x</title><script src="/__zapline/client.js" defer></script></head><body>hi</body></html>"#
/// );
/// ```
pub fn inject(html: &str, nonce: Option<&str>) -> String {
    let is_page = ["<!doctype", "<html", "<head", "<body"]
        .iter()
        .any(|marker| find_ci(html, marker).is_some());
    if !is_page || html.contains(CLIENT_PATH) {
        return html.to_owned();
    }
    let tag = script_tag(nonce);
    let at = find_ci(html, "</head")
        .or_else(|| {
            find_ci(html, "<body")
                .and_then(|start| html[start..].find('>').map(|end| start + end + 1))
        })
        .or_else(|| rfind_ci(html, "</body"))
        .or_else(|| rfind_ci(html, "</html"))
        .unwrap_or(html.len());
    let mut out = String::with_capacity(html.len() + tag.len());
    out.push_str(&html[..at]);
    out.push_str(&tag);
    out.push_str(&html[at..]);
    out
}

/// The nonce the page's Content-Security-Policy allows scripts with, if it
/// uses one (from `script-src-elem`, `script-src`, then `default-src`).
///
/// ```
/// use zapline_cli::dev::inject::csp_nonce;
/// let csp = "default-src 'self'; script-src 'self' 'nonce-r4nd0m'";
/// assert_eq!(csp_nonce(csp).as_deref(), Some("r4nd0m"));
/// assert_eq!(csp_nonce("default-src 'self'"), None);
/// ```
pub fn csp_nonce(csp: &str) -> Option<String> {
    let directive = |name: &str| {
        csp.split(';').map(str::trim).find_map(|d| {
            let mut parts = d.split_ascii_whitespace();
            parts
                .next()
                .filter(|n| n.eq_ignore_ascii_case(name))
                .map(|_| parts.collect::<Vec<_>>())
        })
    };
    let sources = directive("script-src-elem")
        .or_else(|| directive("script-src"))
        .or_else(|| directive("default-src"))?;
    sources.iter().find_map(|s| {
        s.strip_prefix("'nonce-")
            .and_then(|rest| rest.strip_suffix('\''))
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
    })
}

/// Whether a `Content-Type` is an HTML page.
///
/// ```
/// use zapline_cli::dev::inject::is_html;
/// assert!(is_html("text/html; charset=utf-8"));
/// assert!(!is_html("application/json"));
/// ```
pub fn is_html(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/html"))
}

/// `text` without ANSI escape sequences (colour codes in rustc's output).
///
/// ```
/// use zapline_cli::dev::inject::strip_ansi;
/// assert_eq!(strip_ansi("\x1b[1m\x1b[31merror\x1b[0m: x"), "error: x");
/// ```
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ params final-byte (0x40..=0x7e).
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC (hyperlinks): ESC ] ... (BEL | ESC \).
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// Whether `html` (or the start of it) is a whole page, not a fragment such
/// as an htmx partial: it has an `<html>`, `<head>`, `<body>` or doctype.
pub(crate) fn is_page(html: &[u8]) -> bool {
    ["<!doctype", "<html", "<head", "<body"]
        .iter()
        .any(|marker| find_bytes_ci(html, marker).is_some())
}

/// Whether `html` already loads the client.
pub(crate) fn loads_client(html: &[u8]) -> bool {
    html.windows(CLIENT_PATH.len())
        .any(|w| w == CLIENT_PATH.as_bytes())
}

/// Where the client tag goes in the start of a page that is still
/// arriving: before `</head`, else right after the opening `<body>` tag (as
/// [`inject`] does). `None` until one of them has arrived.
pub(crate) fn early_insertion_point(html: &[u8]) -> Option<usize> {
    find_bytes_ci(html, "</head").or_else(|| {
        let start = find_bytes_ci(html, "<body")?;
        html[start..]
            .iter()
            .position(|&b| b == b'>')
            .map(|end| start + end + 1)
    })
}

fn find_bytes_ci(haystack: &[u8], needle: &str) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn rfind_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .rposition(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: &str = r#"<script src="/__zapline/client.js" defer></script>"#;

    #[test]
    fn injects_before_the_end_of_head_ignoring_case() {
        assert_eq!(
            inject("<HTML><HEAD></HEAD><BODY>x</BODY></HTML>", None),
            format!("<HTML><HEAD>{TAG}</HEAD><BODY>x</BODY></HTML>")
        );
    }

    #[test]
    fn falls_back_to_body_then_the_end() {
        assert_eq!(
            inject(r#"<body class="a">x</body>"#, None),
            format!(r#"<body class="a">{TAG}x</body>"#)
        );
        assert_eq!(
            inject("<html>x</html>", None),
            format!("<html>x{TAG}</html>")
        );
        assert_eq!(
            inject("<!DOCTYPE html><p>x", None),
            format!("<!DOCTYPE html><p>x{TAG}")
        );
    }

    #[test]
    fn leaves_fragments_alone() {
        // An htmx partial: htmx would run the script on every swap.
        assert_eq!(inject("<li>hi</li>", None), "<li>hi</li>");
        assert_eq!(inject("", None), "");
    }

    #[test]
    fn does_not_inject_twice() {
        let once = inject("<head></head>", None);
        assert_eq!(inject(&once, None), once);
    }

    #[test]
    fn keeps_multibyte_text_intact() {
        let page = "<head><title>héllo ✓</title></head><body>ünïcode</body>";
        let out = inject(page, None);
        assert!(
            out.starts_with("<head><title>héllo ✓</title><script"),
            "{out}"
        );
        assert!(out.ends_with("</head><body>ünïcode</body>"), "{out}");
    }

    #[test]
    fn copies_the_csp_nonce() {
        let csp = "default-src 'self'; script-src 'nonce-abc123' 'strict-dynamic'";
        let nonce = csp_nonce(csp);
        assert_eq!(nonce.as_deref(), Some("abc123"));
        assert_eq!(
            inject("<head></head>", nonce.as_deref()),
            r#"<head><script src="/__zapline/client.js" defer nonce="abc123"></script></head>"#
        );
        // script-src-elem wins over script-src, which wins over default-src.
        assert_eq!(
            csp_nonce("default-src 'nonce-d'; script-src-elem 'nonce-e'").as_deref(),
            Some("e")
        );
        assert_eq!(csp_nonce("default-src 'nonce-d'").as_deref(), Some("d"));
        assert_eq!(csp_nonce("script-src 'self'; default-src 'nonce-d'"), None);
        assert_eq!(csp_nonce("script-src 'nonce-'"), None);
        assert_eq!(csp_nonce(""), None);
    }

    #[test]
    fn recognises_html_content_types() {
        assert!(is_html("text/html"));
        assert!(is_html("TEXT/HTML;charset=UTF-8"));
        assert!(!is_html("text/html-sandboxed"));
        assert!(!is_html("text/plain"));
        assert!(!is_html(""));
    }

    #[test]
    fn strips_colour_and_hyperlink_escapes() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(
            strip_ansi("\x1b[0m\x1b[1m\x1b[38;5;9merror[E0425]\x1b[0m\x1b[1m: nope\x1b[0m"),
            "error[E0425]: nope"
        );
        assert_eq!(
            strip_ansi("see \x1b]8;;https://x.dev\x1b\\docs\x1b]8;;\x1b\\ now"),
            "see docs now"
        );
    }

    #[test]
    fn the_client_handles_every_event_the_proxy_sends() {
        for event in [
            "hello", "building", "ready", "reload", "css", "problem", "clear",
        ] {
            assert!(
                CLIENT_JS.contains(&format!("\"{event}\"")),
                "client.js does not handle {event}"
            );
        }
        assert!(CLIENT_JS.contains(EVENTS_PATH));
    }
}
