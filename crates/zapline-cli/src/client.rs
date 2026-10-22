//! `zapline client`: a typed TypeScript client generated from the app's
//! OpenAPI document.
//!
//! The output is one dependency-free `.ts` file (uses `fetch`, so it runs in
//! browsers, Node 18+, Deno and Bun): an interface per schema and a
//! `createClient()` whose methods are named after the operation ids and take
//! path parameters, the body and the query as typed arguments. Error
//! responses throw an `ApiError` carrying zapline's `{error, code, details}`.
//!
//! Only the JSON API is included: operations that answer with HTML (htmx
//! pages and fragments) are left out.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

use crate::naming::{pascal_case, snake_case};
use crate::project::Project;
use crate::{openapi, ui};

/// The generated TypeScript and what was left out.
#[derive(Debug)]
pub struct Generated {
    /// The `.ts` source.
    pub source: String,
    /// Operations in the client.
    pub operations: usize,
    /// `METHOD path` of operations skipped because they don't speak JSON.
    pub skipped: Vec<String>,
}

/// `zapline client -o <file>`: export the spec and write the client.
pub fn run(project: &Project, bin: Option<&str>, output: &Path) -> Result<()> {
    let spec_path = project
        .root
        .join("target")
        .join("zapline-client-openapi.json");
    openapi::export(project, bin, &spec_path)?;
    let text = std::fs::read_to_string(&spec_path)
        .with_context(|| format!("cannot read {}", spec_path.display()))?;
    let spec: Value = serde_json::from_str(&text).context("the OpenAPI document is not JSON")?;
    let generated = generate(&spec)?;
    let cwd = std::env::current_dir().context("cannot determine the current directory")?;
    let target = openapi::resolve_output(&cwd, output);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    // Rewrite only on change, so file watchers (vite, tsc --watch) stay quiet.
    if std::fs::read_to_string(&target).ok().as_deref() != Some(generated.source.as_str()) {
        std::fs::write(&target, &generated.source)
            .with_context(|| format!("cannot write {}", target.display()))?;
    }
    ui::status(
        "Wrote",
        format!(
            "{} ({} operations{})",
            output.display(),
            generated.operations,
            if generated.skipped.is_empty() {
                String::new()
            } else {
                format!(", {} HTML routes skipped", generated.skipped.len())
            }
        ),
    );
    Ok(())
}

fn spec_path(project: &Project) -> std::path::PathBuf {
    project
        .root
        .join("target")
        .join("zapline-client-openapi.json")
}

/// Generates the client from the spec at `spec_path` into `target`. The file
/// is only rewritten when it changes, so watchers (vite, `tsc --watch`) stay
/// quiet; the flag says whether it did.
fn write(spec_path: &Path, target: &Path) -> Result<(Generated, bool)> {
    let text = std::fs::read_to_string(spec_path)
        .with_context(|| format!("cannot read {}", spec_path.display()))?;
    let spec: Value = serde_json::from_str(&text).context("the OpenAPI document is not JSON")?;
    let generated = generate(&spec)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let changed =
        std::fs::read_to_string(target).ok().as_deref() != Some(generated.source.as_str());
    if changed {
        std::fs::write(target, &generated.source)
            .with_context(|| format!("cannot write {}", target.display()))?;
    }
    Ok((generated, changed))
}

/// `zapline dev --client`: regenerate from a freshly built executable
/// (faster than `cargo run`). Returns whether the file changed.
pub fn regenerate(project: &Project, exe: &Path, target: &Path) -> Result<bool> {
    let spec_path = spec_path(project);
    let _ = std::fs::remove_file(&spec_path);
    let mut cmd = std::process::Command::new(exe);
    cmd.current_dir(&project.root)
        .env(openapi::ENV_EXPORT_OPENAPI, &spec_path);
    let captured = crate::build::run_captured(cmd, std::time::Duration::from_secs(30))?;
    if !captured.status.success() {
        return Err(captured.failure("before exporting the spec"));
    }
    Ok(write(&spec_path, target)?.1)
}

/// Converts an OpenAPI 3.1 document into a TypeScript client.
pub fn generate(spec: &Value) -> Result<Generated> {
    let Some(paths) = spec.get("paths").and_then(Value::as_object) else {
        bail!("the OpenAPI document has no paths");
    };
    let empty = Map::new();
    let schemas = spec
        .pointer("/components/schemas")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    let title = spec
        .pointer("/info/title")
        .and_then(Value::as_str)
        .unwrap_or("API");
    let version = spec
        .pointer("/info/version")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut out = String::new();
    let _ = writeln!(
        out,
        "// {title} {version} client, generated by `zapline client` from the OpenAPI document.\n\
         // Do not edit: regenerate with `zapline client` (or let `zapline dev --client` do it).\n\
         /* eslint-disable */"
    );

    // Types.
    for (name, schema) in schemas {
        out.push('\n');
        doc_comment(
            &mut out,
            "",
            schema.get("description").and_then(Value::as_str),
        );
        let ident = type_name(name);
        if is_object(schema) {
            let _ = writeln!(out, "export interface {ident} {}", object_body(schema, ""));
        } else {
            let _ = writeln!(out, "export type {ident} = {};", ts_type(schema, ""));
        }
    }

    out.push_str(RUNTIME);
    out.push_str("\nexport function createClient(options: ClientOptions = {}) {\n  const request = makeRequest(options);\n  return {\n");

    let mut operations = 0;
    let mut skipped = Vec::new();
    let mut used = BTreeSet::new();
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        let shared = item.get("parameters");
        for method in ["get", "post", "put", "patch", "delete", "head", "options"] {
            let Some(op) = item.get(method) else { continue };
            match operation(path, method, op, shared, &mut used)? {
                Some(code) => {
                    out.push_str(&code);
                    operations += 1;
                }
                None => skipped.push(format!("{} {path}", method.to_uppercase())),
            }
        }
    }
    out.push_str("  };\n}\n\nexport type Client = ReturnType<typeof createClient>;\n");
    Ok(Generated {
        source: out,
        operations,
        skipped,
    })
}

/// Shared by every generated client: options, `ApiError` and the `fetch` wrapper.
const RUNTIME: &str = r#"
export interface ClientOptions {
  /** Prefix for every path, e.g. "https://api.example.com". Default: same origin. */
  baseUrl?: string;
  /** Extra headers (for example `Authorization`), or a function returning them. */
  headers?: HeadersInit | (() => HeadersInit | Promise<HeadersInit>);
  /** A `fetch` implementation. Default: the global one. */
  fetch?: typeof fetch;
}

/** A non-2xx response. `code` and `details` come from zapline's error body. */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string | undefined;
  readonly details: unknown;
  readonly body: unknown;
  constructor(status: number, body: unknown) {
    const e = (typeof body === "object" && body ? body : {}) as { error?: unknown; code?: string; details?: unknown };
    super(e.error !== undefined ? String(e.error) : `HTTP ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
    this.code = e.code;
    this.details = e.details;
  }
}

type Query = Record<string, string | number | boolean | null | undefined | Array<string | number | boolean>>;

function makeRequest(options: ClientOptions) {
  const base = (options.baseUrl ?? "").replace(/\/$/, "");
  return async function request<T>(
    method: string,
    path: string,
    init: { query?: Query | undefined; json?: unknown; form?: Record<string, unknown> | undefined } = {},
  ): Promise<T> {
    let url = base + path;
    if (init.query) {
      const qs = new URLSearchParams();
      for (const [k, v] of Object.entries(init.query)) {
        if (v === undefined || v === null) continue;
        for (const item of Array.isArray(v) ? v : [v]) qs.append(k, String(item));
      }
      const s = qs.toString();
      if (s) url += "?" + s;
    }
    const extra = typeof options.headers === "function" ? await options.headers() : options.headers;
    const headers = new Headers(extra);
    let body: BodyInit | null = null;
    if (init.json !== undefined) {
      headers.set("content-type", "application/json");
      body = JSON.stringify(init.json);
    } else if (init.form !== undefined) {
      const form = new URLSearchParams();
      for (const [k, v] of Object.entries(init.form)) if (v !== undefined && v !== null) form.append(k, String(v));
      body = form;
    }
    if (!headers.has("accept")) headers.set("accept", "application/json");
    const res = await (options.fetch ?? fetch)(url, { method, headers, body });
    const text = await res.text();
    const type = res.headers.get("content-type") ?? "";
    const data = text && type.includes("json") ? JSON.parse(text) : text || undefined;
    if (!res.ok) throw new ApiError(res.status, data);
    return data as T;
  };
}
"#;

fn operation(
    path: &str,
    method: &str,
    op: &Value,
    shared_params: Option<&Value>,
    used: &mut BTreeSet<String>,
) -> Result<Option<String>> {
    // Success response: the lowest 2xx.
    let responses = op.get("responses").and_then(Value::as_object);
    let success = responses.and_then(|r| {
        r.iter()
            .filter(|(code, _)| code.starts_with('2'))
            .min_by_key(|(code, _)| code.as_str())
            .map(|(_, v)| v)
    });
    let content = success
        .and_then(|s| s.get("content"))
        .and_then(Value::as_object);
    let result = match content {
        None => "void".to_string(),
        Some(c) if c.is_empty() => "void".to_string(),
        Some(c) => match c.get("application/json") {
            Some(media) => media
                .get("schema")
                .map_or("unknown".into(), |s| ts_type(s, "    ")),
            None if c.keys().any(|k| k.starts_with("text/html")) => return Ok(None),
            None => "string".to_string(),
        },
    };

    // Request body.
    let body = op
        .pointer("/requestBody/content")
        .and_then(Value::as_object);
    let (body_kind, body_type) = match body {
        None => (None, String::new()),
        Some(c) => {
            if let Some(m) = c.get("application/json") {
                (
                    Some("json"),
                    m.get("schema")
                        .map_or("unknown".into(), |s| ts_type(s, "    ")),
                )
            } else if let Some(m) = c.get("application/x-www-form-urlencoded") {
                (
                    Some("form"),
                    m.get("schema")
                        .map_or("Record<string, unknown>".into(), |s| ts_type(s, "    ")),
                )
            } else {
                // Multipart and raw bodies: not generated.
                return Ok(None);
            }
        }
    };
    let body_required = op
        .pointer("/requestBody/required")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Parameters: operation-level ones override path-item ones by name+in.
    let mut params: Vec<&Value> = Vec::new();
    for p in op
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            shared_params
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
    {
        let key = (p.get("name"), p.get("in"));
        if !params.iter().any(|q| (q.get("name"), q.get("in")) == key) {
            params.push(p);
        }
    }
    let of = |kind: &str| -> Vec<&Value> {
        params
            .iter()
            .copied()
            .filter(|p| p.get("in").and_then(Value::as_str) == Some(kind))
            .collect()
    };
    let path_params = of("path");
    let query_params = of("query");

    // Name.
    let id = op
        .get("operationId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{method}_{path}"));
    let mut name = camel_case(&id);
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        name = format!("op{name}");
    }
    while !used.insert(name.clone()) {
        name.push('_');
    }

    // Arguments: path params in path order, then body, then query.
    let mut args = Vec::new();
    let mut url = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        url.push_str(&rest[..start]);
        let raw = &rest[start + 1..start + len];
        let param = raw.trim_start_matches('*');
        let ident = js_ident(param);
        let ty = path_params
            .iter()
            .find(|p| {
                p.get("name").and_then(Value::as_str) == Some(raw)
                    || p.get("name").and_then(Value::as_str) == Some(param)
            })
            .and_then(|p| p.get("schema"))
            .map_or("string".to_string(), |s| ts_type(s, ""));
        args.push(format!("{ident}: {ty}"));
        if raw.starts_with('*') {
            // Catch-all: keep slashes.
            let _ = write!(
                url,
                "${{String({ident}).split(\"/\").map(encodeURIComponent).join(\"/\")}}"
            );
        } else {
            let _ = write!(url, "${{encodeURIComponent(String({ident}))}}");
        }
        rest = &rest[start + len + 1..];
    }
    url.push_str(rest);

    let mut init = Vec::new();
    if let Some(kind) = body_kind {
        let optional = if body_required { "" } else { "?" };
        args.push(format!("body{optional}: {body_type}"));
        init.push(format!("{kind}: body"));
    }
    if !query_params.is_empty() {
        let mut fields = Vec::new();
        let mut any_required = false;
        for p in &query_params {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("q");
            let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
            any_required |= required;
            let ty = p.get("schema").map_or("string".into(), |s| ts_type(s, ""));
            fields.push(format!(
                "{}{}: {ty}",
                prop_name(name),
                if required { "" } else { "?" }
            ));
        }
        args.push(format!(
            "query{}: {{ {} }}",
            if any_required { "" } else { "?" },
            fields.join("; ")
        ));
        init.push("query".to_string());
    }

    let mut code = String::new();
    let summary = op.get("summary").and_then(Value::as_str);
    let description = op.get("description").and_then(Value::as_str);
    let mut doc = format!("`{} {path}`", method.to_uppercase());
    if let Some(s) = summary {
        doc = format!("{s}\n\n{doc}");
    }
    if let Some(d) = description.filter(|d| Some(*d) != summary) {
        doc = format!("{doc}\n\n{d}");
    }
    if op.get("deprecated").and_then(Value::as_bool) == Some(true) {
        doc.push_str("\n\n@deprecated");
    }
    doc_comment(&mut code, "    ", Some(&doc));
    let init = if init.is_empty() {
        String::new()
    } else {
        format!(", {{ {} }}", init.join(", "))
    };
    let _ = writeln!(
        code,
        "    {name}: ({}) =>\n      request<{result}>(\"{}\", `{url}`{init}),",
        args.join(", "),
        method.to_uppercase()
    );
    Ok(Some(code))
}

// ---------------------------------------------------------------------------
// JSON Schema -> TypeScript

fn is_object(schema: &Value) -> bool {
    schema.get("properties").is_some()
        && schema.get("oneOf").is_none()
        && schema.get("anyOf").is_none()
        && schema.get("allOf").is_none()
}

fn object_body(schema: &Value, indent: &str) -> String {
    let required: BTreeSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut out = String::from("{\n");
    let inner = format!("{indent}  ");
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (name, prop) in props {
            doc_comment(
                &mut out,
                &inner,
                prop.get("description").and_then(Value::as_str),
            );
            let _ = writeln!(
                out,
                "{inner}{}{}: {};",
                prop_name(name),
                if required.contains(name.as_str()) {
                    ""
                } else {
                    "?"
                },
                ts_type(prop, &inner)
            );
        }
    }
    match schema.get("additionalProperties") {
        Some(Value::Bool(true)) => {
            let _ = writeln!(out, "{inner}[key: string]: unknown;");
        }
        Some(extra @ Value::Object(_)) => {
            let _ = writeln!(out, "{inner}[key: string]: {};", ts_type(extra, &inner));
        }
        _ => {}
    }
    out.push_str(indent);
    out.push('}');
    out
}

/// The TypeScript type of a JSON Schema.
pub fn ts_type(schema: &Value, indent: &str) -> String {
    let Some(obj) = schema.as_object() else {
        // `true` accepts anything, `false` nothing.
        return if schema == &Value::Bool(false) {
            "never".into()
        } else {
            "unknown".into()
        };
    };
    if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
        return type_name(r.rsplit('/').next().unwrap_or(r));
    }
    if let Some(c) = obj.get("const") {
        return c.to_string();
    }
    if let Some(values) = obj.get("enum").and_then(Value::as_array) {
        return union(values.iter().map(Value::to_string).collect());
    }
    for (key, sep) in [("oneOf", " | "), ("anyOf", " | "), ("allOf", " & ")] {
        if let Some(parts) = obj.get(key).and_then(Value::as_array) {
            let mut types: Vec<String> = parts.iter().map(|p| wrap(ts_type(p, indent))).collect();
            types.dedup();
            // Sibling properties (an object with a discriminator, say).
            if obj.contains_key("properties") {
                types.push(object_body(schema, indent));
            }
            return if sep == " | " {
                union(types)
            } else {
                types.join(sep)
            };
        }
    }
    let types: Vec<&str> = match obj.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ if obj.contains_key("properties") => vec!["object"],
        _ => return "unknown".into(),
    };
    let ts: Vec<String> = types
        .iter()
        .map(|t| match *t {
            "string" => "string".to_string(),
            "integer" | "number" => "number".to_string(),
            "boolean" => "boolean".to_string(),
            "null" => "null".to_string(),
            "array" => array_type(obj, indent),
            "object" if obj.contains_key("properties") => object_body(schema, indent),
            "object" => match obj.get("additionalProperties") {
                Some(extra @ Value::Object(_)) => {
                    format!("Record<string, {}>", ts_type(extra, indent))
                }
                _ => "Record<string, unknown>".to_string(),
            },
            _ => "unknown".to_string(),
        })
        .collect();
    union(ts)
}

fn array_type(obj: &Map<String, Value>, indent: &str) -> String {
    if let Some(items) = obj.get("prefixItems").and_then(Value::as_array) {
        let parts: Vec<String> = items.iter().map(|i| ts_type(i, indent)).collect();
        return format!("[{}]", parts.join(", "));
    }
    match obj.get("items") {
        Some(items) => format!("{}[]", wrap(ts_type(items, indent))),
        None => "unknown[]".into(),
    }
}

fn union(mut types: Vec<String>) -> String {
    let mut seen = BTreeSet::new();
    types.retain(|t| seen.insert(t.clone()));
    if types.is_empty() {
        "never".into()
    } else {
        types.join(" | ")
    }
}

/// Parenthesizes unions and intersections used as array items or parts.
fn wrap(t: String) -> String {
    if (t.contains(" | ") || t.contains(" & ")) && !t.starts_with('{') {
        format!("({t})")
    } else {
        t
    }
}

fn doc_comment(out: &mut String, indent: &str, text: Option<&str>) {
    let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
        return;
    };
    let text = text.replace("*/", "*\\/");
    if !text.contains('\n') {
        let _ = writeln!(out, "{indent}/** {text} */");
        return;
    }
    let _ = writeln!(out, "{indent}/**");
    for line in text.lines() {
        let _ = writeln!(
            out,
            "{indent} *{}{line}",
            if line.is_empty() { "" } else { " " }
        );
    }
    let _ = writeln!(out, "{indent} */");
}

/// Global TypeScript/DOM names a schema must not shadow.
const RESERVED_TYPES: &[&str] = &[
    "Array",
    "Blob",
    "Boolean",
    "Client",
    "ClientOptions",
    "Date",
    "Event",
    "File",
    "FormData",
    "Headers",
    "Map",
    "Number",
    "Object",
    "Omit",
    "Partial",
    "Pick",
    "Promise",
    "Query",
    "Record",
    "Request",
    "Response",
    "Set",
    "String",
    "Symbol",
    "URL",
];

/// The TypeScript name of a schema. zapline's `Error` body becomes
/// `ErrorBody` so the global `Error` class stays usable.
fn type_name(name: &str) -> String {
    let n = pascal_case(&name.replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
    if n.is_empty() || n.starts_with(|c: char| c.is_ascii_digit()) {
        format!("T{n}")
    } else if n == "Error" || n == "ApiError" {
        format!("{n}Body")
    } else if RESERVED_TYPES.contains(&n.as_str()) {
        format!("{n}Schema")
    } else {
        n
    }
}

fn camel_case(s: &str) -> String {
    let snake = snake_case(&s.replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
    let mut out = String::new();
    for (i, part) in snake.split('_').filter(|p| !p.is_empty()).enumerate() {
        if i == 0 {
            out.push_str(part);
        } else {
            let mut cs = part.chars();
            if let Some(c) = cs.next() {
                out.push(c.to_ascii_uppercase());
                out.push_str(cs.as_str());
            }
        }
    }
    out
}

/// A valid JS identifier for a parameter.
fn js_ident(s: &str) -> String {
    let id = camel_case(s);
    match id.as_str() {
        "" => "param".into(),
        "default" | "delete" | "new" | "class" | "function" | "var" | "let" | "const" | "in"
        | "for" | "if" | "return" | "switch" | "this" | "typeof" | "void" | "with" => {
            format!("{id}_")
        }
        _ if id.starts_with(|c: char| c.is_ascii_digit()) => format!("p{id}"),
        _ => id,
    }
}

/// An object key: bare when it is an identifier, quoted otherwise.
fn prop_name(name: &str) -> String {
    let ident = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ident {
        name.to_string()
    } else {
        Value::from(name).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_types() {
        assert_eq!(
            ts_type(&json!({"type": ["string", "null"]}), ""),
            "string | null"
        );
        assert_eq!(
            ts_type(
                &json!({"type": "array", "items": {"$ref": "#/components/schemas/User"}}),
                ""
            ),
            "User[]"
        );
        assert_eq!(
            ts_type(
                &json!({"type": "array", "items": {"anyOf": [{"$ref": "#/components/schemas/A"}, {"type": "null"}]}}),
                ""
            ),
            "(A | null)[]"
        );
        assert_eq!(ts_type(&json!({"enum": ["a", "b"]}), ""), "\"a\" | \"b\"");
        assert_eq!(
            ts_type(
                &json!({"type": "object", "additionalProperties": {"type": "integer"}}),
                ""
            ),
            "Record<string, number>"
        );
        assert_eq!(
            ts_type(
                &json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}]}),
                ""
            ),
            "[string, number]"
        );
        assert_eq!(ts_type(&json!({}), ""), "unknown");
        assert_eq!(
            ts_type(
                &json!({"type": "object", "properties": {"a-b": {"type": "string"}, "n": {"type": "integer"}}, "required": ["n"]}),
                ""
            ),
            "{\n  \"a-b\"?: string;\n  n: number;\n}"
        );
    }

    #[test]
    fn names() {
        assert_eq!(camel_case("get_one_get"), "getOneGet");
        assert_eq!(camel_case("users_list"), "usersList");
        assert_eq!(camel_case("allArgs"), "allArgs");
        assert_eq!(js_ident("user_id"), "userId");
        assert_eq!(js_ident("default"), "default_");
        assert_eq!(type_name("Page_User"), "PageUser");
        assert_eq!(type_name("Error"), "ErrorBody");
        assert_eq!(type_name("Response"), "ResponseSchema");
    }

    #[test]
    fn operations_with_path_query_and_body() {
        let spec = json!({
            "openapi": "3.1.0",
            "info": {"title": "demo", "version": "1"},
            "paths": {
                "/users/{id}": {
                    "get": {"operationId": "get_user", "summary": "One user.",
                        "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}],
                        "responses": {"200": {"description": "", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}}}}},
                    "put": {"operationId": "replace",
                        "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}],
                        "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}}},
                        "responses": {"204": {"description": ""}}}
                },
                "/users": {"get": {"operationId": "list",
                    "parameters": [{"name": "limit", "in": "query", "schema": {"type": "integer"}}],
                    "responses": {"200": {"description": "", "content": {"application/json": {"schema": {"type": "array", "items": {"$ref": "#/components/schemas/User"}}}}}}}},
                "/": {"get": {"operationId": "page", "responses": {"200": {"description": "", "content": {"text/html": {}}}}}},
                "/files/{*path}": {"get": {"operationId": "file", "responses": {"200": {"description": "", "content": {"text/plain": {}}}}}}
            },
            "components": {"schemas": {"User": {"type": "object", "description": "A user.", "required": ["id"], "properties": {"id": {"type": "integer"}, "name": {"type": ["string", "null"]}}}}}
        });
        let g = generate(&spec).unwrap();
        assert_eq!(g.operations, 4);
        assert_eq!(g.skipped, vec!["GET /"]);
        let s = &g.source;
        assert!(s.contains("/** A user. */\nexport interface User {\n  id: number;\n  name?: string | null;\n}"), "{s}");
        assert!(s.contains("getUser: (id: number) =>\n      request<User>(\"GET\", `/users/${encodeURIComponent(String(id))}`),"), "{s}");
        assert!(s.contains("replace: (id: number, body: User) =>\n      request<void>(\"PUT\", `/users/${encodeURIComponent(String(id))}`, { json: body }),"), "{s}");
        assert!(s.contains("list: (query?: { limit?: number }) =>\n      request<User[]>(\"GET\", `/users`, { query }),"), "{s}");
        assert!(s.contains("file: (path: string) =>\n      request<string>(\"GET\", `/files/${String(path).split(\"/\").map(encodeURIComponent).join(\"/\")}`),"), "{s}");
        assert!(
            s.contains("/**\n     * One user.\n     *\n     * `GET /users/{id}`\n     */"),
            "{s}"
        );
    }
}
