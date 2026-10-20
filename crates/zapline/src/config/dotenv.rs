//! A small `.env` parser.
//!
//! Supported: `KEY=value`, `export KEY=value`, `# comments`, blank lines,
//! inline comments after unquoted values (` # ...`), `'single'` quotes
//! (literal), and `"double"` quotes with `\n \r \t \" \\ \$` escapes. Quoted
//! values may span lines. Variable expansion (`${VAR}`) is not performed.

/// Parse `src` into `(key, value)` pairs, in file order. Errors carry a
/// 1-based line number.
pub(crate) fn parse(src: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut lines = src.lines().enumerate();
    while let Some((index, line)) = lines.next() {
        let lineno = index + 1;
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((key, rest)) = line.split_once('=') else {
            return Err(format!("line {lineno}: expected `KEY=value`"));
        };
        let key = key.trim();
        if !valid_key(key) {
            return Err(format!("line {lineno}: invalid variable name `{key}`"));
        }
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let mut text = rest[1..].to_owned();
                loop {
                    if let Some((value, tail)) = scan_quoted(&text, quote) {
                        let tail = tail.trim_start();
                        if !(tail.is_empty() || tail.starts_with('#')) {
                            return Err(format!(
                                "line {lineno}: unexpected text after the closing quote"
                            ));
                        }
                        break value;
                    }
                    match lines.next() {
                        Some((_, next)) => {
                            text.push('\n');
                            text.push_str(next);
                        }
                        None => return Err(format!("line {lineno}: unterminated quoted value")),
                    }
                }
            }
            _ => unquoted(rest),
        };
        out.push((key.to_owned(), value));
    }
    Ok(out)
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// An unquoted value: up to an inline ` #` comment, trimmed.
fn unquoted(rest: &str) -> String {
    let mut end = rest.len();
    let bytes = rest.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            end = i;
            break;
        }
    }
    rest[..end].trim_end().to_owned()
}

/// Scan a quoted value whose opening quote was already consumed. Returns the
/// unescaped value and the text after the closing quote, or `None` if the
/// closing quote is not in `text`.
fn scan_quoted(text: &str, quote: char) -> Option<(String, &str)> {
    let mut value = String::with_capacity(text.len());
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == quote {
            return Some((value, &text[i + c.len_utf8()..]));
        }
        if c == '\\' && quote == '"' {
            match chars.next() {
                Some((_, 'n')) => value.push('\n'),
                Some((_, 'r')) => value.push('\r'),
                Some((_, 't')) => value.push('\t'),
                Some((_, e @ ('"' | '\\' | '$' | '\''))) => value.push(e),
                Some((_, other)) => {
                    value.push('\\');
                    value.push(other);
                }
                None => value.push('\\'),
            }
            continue;
        }
        value.push(c);
    }
    None
}
