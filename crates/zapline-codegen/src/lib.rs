//! Typed API clients from zapline's OpenAPI 3.1 documents.
//!
//! Two outputs from the same document:
//!
//! - [`typescript`]: one dependency-free `.ts` file for a bundled frontend
//!   (React, Vue, Svelte, ...): an interface per schema and a
//!   `createClient()` whose methods are named after the operation ids.
//! - [`javascript`]: the same client as a plain ES module with JSDoc types,
//!   for pages without a build step. Editors and `tsc --checkJs` read the
//!   JSDoc, so `<script type="module">` code gets autocomplete and type
//!   errors too. zapline serves it at `/_zapline/api.js` with
//!   `OpenApi::client_js()`.
//!
//! Both use `fetch` (browsers, Node 18+, Deno, Bun), take path parameters,
//! the body and the query as typed arguments, and throw `ApiError` for
//! error responses. A `422` with field errors becomes a `ValidationError`
//! whose `fields` are keyed by the request body's field names, so
//! `isValidationError(e, "createTask") && e.fields.title` type-checks. In a
//! browser, unsafe requests echo zapline's CSRF cookie automatically.
//!
//! ```
//! let spec = serde_json::json!({
//!     "openapi": "3.1.0",
//!     "info": {"title": "demo", "version": "1"},
//!     "paths": {"/health": {"get": {"operationId": "health",
//!         "responses": {"200": {"description": "OK",
//!             "content": {"text/plain": {"schema": {"type": "string"}}}}}}}}
//! });
//! let ts = zapline_codegen::typescript(&spec).unwrap();
//! assert!(ts.source.contains("health: (options?: CallOptions) =>"));
//! let js = zapline_codegen::javascript(&spec).unwrap();
//! assert!(js.source.contains("@returns {Promise<string>}"));
//! ```

mod names;
mod ops;
mod schema;

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use serde_json::{Map, Value};

use crate::ops::{one_line, Arg, Method};
use crate::schema::{doc_comment, escape_comment, facts, is_interface, schema_doc, Types};

/// The runtime every TypeScript client starts with.
const RUNTIME_TS: &str = include_str!("runtime.ts");
/// The runtime every JavaScript client starts with.
const RUNTIME_JS: &str = include_str!("runtime.js");

/// A generated client.
///
/// ```
/// # let spec = serde_json::json!({"paths": {}});
/// let client = zapline_codegen::typescript(&spec).unwrap();
/// assert_eq!(client.operations, 0);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// The source text.
    pub source: String,
    /// Methods in the client.
    pub operations: usize,
    /// `METHOD path` of operations left out because they answer with HTML.
    pub skipped: Vec<String>,
    /// Method name -> its one-line TypeScript signature, for
    /// [`changes_since`](Self::changes_since).
    pub signatures: BTreeMap<String, String>,
}

/// How a client's methods differ from a previous version.
///
/// ```
/// # let spec = serde_json::json!({"paths": {}});
/// let a = zapline_codegen::typescript(&spec).unwrap();
/// assert!(a.changes_since(&a).is_empty());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// New methods.
    pub added: Vec<String>,
    /// Methods that are gone.
    pub removed: Vec<String>,
    /// Methods whose arguments or result changed.
    pub changed: Vec<String>,
}

impl Changes {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// `+added -removed ~changed`, e.g. `+createTask ~getTask`.
impl fmt::Display for Changes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        parts.extend(self.added.iter().map(|m| format!("+{m}")));
        parts.extend(self.removed.iter().map(|m| format!("-{m}")));
        parts.extend(self.changed.iter().map(|m| format!("~{m}")));
        f.write_str(&parts.join(" "))
    }
}

impl Generated {
    /// Methods added, removed or changed since `previous`.
    pub fn changes_since(&self, previous: &Generated) -> Changes {
        let mut c = Changes::default();
        for (name, sig) in &self.signatures {
            match previous.signatures.get(name) {
                None => c.added.push(name.clone()),
                Some(old) if old != sig => c.changed.push(name.clone()),
                Some(_) => {}
            }
        }
        for name in previous.signatures.keys() {
            if !self.signatures.contains_key(name) {
                c.removed.push(name.clone());
            }
        }
        c
    }
}

/// Why a document could not be turned into a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// The parts of a document both languages need.
struct Doc<'a> {
    paths: &'a Map<String, Value>,
    schemas: &'a Map<String, Value>,
    title: &'a str,
    version: &'a str,
}

fn doc(spec: &Value) -> Result<Doc<'_>, Error> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    let empty = EMPTY.get_or_init(Map::new);
    let Some(paths) = spec.get("paths").and_then(Value::as_object) else {
        return Err(Error("the OpenAPI document has no paths".into()));
    };
    Ok(Doc {
        paths,
        schemas: spec
            .pointer("/components/schemas")
            .and_then(Value::as_object)
            .unwrap_or(empty),
        title: spec
            .pointer("/info/title")
            .and_then(Value::as_str)
            .unwrap_or("API"),
        version: spec
            .pointer("/info/version")
            .and_then(Value::as_str)
            .unwrap_or(""),
    })
}

fn header(out: &mut String, d: &Doc<'_>, what: &str) {
    let title = one_line(&format!("{} {}", d.title, d.version));
    let _ = writeln!(
        out,
        "// {title} {what}, generated by zapline from the OpenAPI document.\n\
         // Do not edit: `zapline dev` regenerates it after every build (or run `zapline client`).\n\
         /* eslint-disable */"
    );
}

/// Whether the argument at `i` must be spelled `x: T | undefined` instead
/// of `x?: T` because a required argument follows it.
fn optional_before_required(args: &[Arg], i: usize) -> bool {
    args[i].optional && args[i + 1..].iter().any(|a| !a.optional)
}

fn signature(m: &Method) -> String {
    let args: Vec<String> = m
        .args
        .iter()
        .map(|a| {
            format!(
                "{}{}: {}",
                a.name,
                if a.optional { "?" } else { "" },
                one_line(&a.ty)
            )
        })
        .collect();
    format!("({}) => {}", args.join(", "), one_line(&m.result))
}

fn signatures(methods: &[Method]) -> BTreeMap<String, String> {
    methods
        .iter()
        .map(|m| (m.name.clone(), signature(m)))
        .collect()
}

/// `@throws` lines for a method's documented errors.
fn throws(m: &Method) -> Vec<String> {
    m.errors
        .iter()
        .map(|(code, d)| {
            let class = match (&m.fields, code.as_str()) {
                (Some(f), "422") => format!("ValidationError<{f}>"),
                _ => "ApiError".into(),
            };
            let d = one_line(d);
            format!(
                "@throws {{{class}}} `{code}`{}{d}",
                if d.is_empty() { "" } else { " " }
            )
        })
        .collect()
}

fn request_call(m: &Method, ts_result: Option<&str>) -> String {
    let init = if m.init.is_empty() {
        "{}".to_string()
    } else {
        format!("{{ {} }}", m.init.join(", "))
    };
    let generic = ts_result.map(|r| format!("<{r}>")).unwrap_or_default();
    format!(
        "request{generic}({}, \"{}\", `{}`, {init}, options)",
        Value::from(m.name.as_str()),
        m.http,
        m.url
    )
}

/// Converts an OpenAPI 3.1 document into a TypeScript client.
///
/// ```
/// let spec = serde_json::json!({
///     "openapi": "3.1.0",
///     "info": {"title": "demo", "version": "1"},
///     "paths": {"/users/{id}": {"get": {"operationId": "get_user",
///         "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}],
///         "responses": {"200": {"description": "OK", "content": {"application/json":
///             {"schema": {"$ref": "#/components/schemas/User"}}}}}}}},
///     "components": {"schemas": {"User": {"type": "object", "required": ["id"],
///         "properties": {"id": {"type": "integer"}}}}}
/// });
/// let ts = zapline_codegen::typescript(&spec).unwrap();
/// assert!(ts.source.contains("export interface User {\n  id: number;\n}"));
/// assert!(ts.source.contains("getUser: (id: number, options?: CallOptions) =>"));
/// ```
pub fn typescript(spec: &Value) -> Result<Generated, Error> {
    let d = doc(spec)?;
    let types = Types::new(d.schemas, true);
    let mut out = String::new();
    header(&mut out, &d, "client");

    for (_, name, schema) in types.named() {
        out.push('\n');
        doc_comment(&mut out, "", schema_doc(schema).as_deref());
        if is_interface(schema) {
            let _ = writeln!(out, "export interface {name} {}", types.object(schema, ""));
        } else {
            let _ = writeln!(out, "export type {name} = {};", types.ts(schema, ""));
        }
    }

    out.push_str(RUNTIME_TS);
    let (methods, skipped) = ops::methods(&types, d.paths, "    ");

    out.push_str("\n/** Field names a `422` validation error of each method can report. */\n");
    out.push_str("export interface ValidationFields {");
    let mut any = false;
    for m in &methods {
        if let Some(f) = &m.fields {
            let _ = write!(out, "\n  {}: {f};", m.name);
            any = true;
        }
    }
    out.push_str(if any { "\n}\n" } else { "}\n" });

    out.push_str(
        "\n/** A client for the API. Each method resolves to the response body. */\n\
         export function createClient(options: ClientOptions = {}) {\n  \
         const request = makeRequest(options);\n  return {\n",
    );
    for m in &methods {
        let mut doc = m.doc.clone();
        let params: Vec<String> = m
            .args
            .iter()
            .filter_map(|a| Some(format!("@param {} {}", a.name, one_line(a.doc.as_deref()?))))
            .collect();
        let throws = throws(m);
        if !params.is_empty() || !throws.is_empty() {
            doc.push('\n');
            for line in params.iter().chain(&throws) {
                doc.push('\n');
                doc.push_str(line);
            }
        }
        doc_comment(&mut out, "    ", Some(&doc));
        let mut args: Vec<String> = m
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if optional_before_required(&m.args, i) {
                    format!("{}: {} | undefined", a.name, a.ty)
                } else {
                    format!("{}{}: {}", a.name, if a.optional { "?" } else { "" }, a.ty)
                }
            })
            .collect();
        args.push("options?: CallOptions".into());
        let _ = writeln!(
            out,
            "    {}: ({}) =>\n      {},",
            m.name,
            args.join(", "),
            request_call(m, Some(&m.result))
        );
    }
    out.push_str(
        "  };\n}\n\n\
         /** The type of {@link createClient}'s result. */\n\
         export type Client = ReturnType<typeof createClient>;\n\n\
         /** A same-origin client with the default options. */\n\
         export const api: Client = createClient();\n",
    );
    Ok(Generated {
        source: out,
        operations: methods.len(),
        skipped,
        signatures: signatures(&methods),
    })
}

/// Converts an OpenAPI 3.1 document into an ES module with JSDoc types.
///
/// ```
/// let spec = serde_json::json!({
///     "openapi": "3.1.0",
///     "info": {"title": "demo", "version": "1"},
///     "paths": {"/users/{id}": {"get": {"operationId": "get_user",
///         "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}],
///         "responses": {"200": {"description": "OK", "content": {"application/json":
///             {"schema": {"$ref": "#/components/schemas/User"}}}}}}}},
///     "components": {"schemas": {"User": {"type": "object", "required": ["id"],
///         "properties": {"id": {"type": "integer"}}}}}
/// });
/// let js = zapline_codegen::javascript(&spec).unwrap();
/// assert!(js.source.contains(" * @typedef {object} User\n * @property {number} id\n"));
/// assert!(js.source.contains("getUser: (id, options) =>"));
/// ```
pub fn javascript(spec: &Value) -> Result<Generated, Error> {
    let d = doc(spec)?;
    let types = Types::new(d.schemas, false);
    let mut out = String::new();
    header(&mut out, &d, "client module");
    out.push_str("// @ts-check\n");

    for (_, name, schema) in types.named() {
        out.push('\n');
        let mut lines: Vec<String> = schema_doc(schema)
            .map(|d| d.lines().map(String::from).collect())
            .unwrap_or_default();
        let props = schema.get("properties").and_then(Value::as_object);
        let plain = is_interface(schema)
            && props.is_some_and(|p| p.keys().all(|k| names::is_ident(k)))
            && schema
                .get("additionalProperties")
                .is_none_or(|a| a == &Value::Bool(false));
        match props {
            Some(props) if plain => {
                let required: Vec<&str> = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                lines.push(format!("@typedef {{object}} {name}"));
                for (key, prop) in props {
                    let key = if required.contains(&key.as_str()) {
                        key.clone()
                    } else {
                        format!("[{key}]")
                    };
                    let text = property_text(prop);
                    lines.push(
                        format!("@property {{{}}} {key} {text}", types.ts(prop, ""))
                            .trim_end()
                            .to_string(),
                    );
                }
            }
            _ => lines.push(format!("@typedef {{{}}} {name}", types.ts(schema, ""))),
        }
        jsdoc(&mut out, "", &lines);
    }

    out.push_str(RUNTIME_JS);
    let (methods, skipped) = ops::methods(&types, d.paths, "");

    out.push('\n');
    let mut lines =
        vec!["Field names a `422` validation error of each method can report.".to_string()];
    let fields: Vec<&Method> = methods.iter().filter(|m| m.fields.is_some()).collect();
    if fields.is_empty() {
        lines.push("@typedef {{}} ValidationFields".into());
    } else {
        lines.push("@typedef {object} ValidationFields".into());
        for m in fields {
            lines.push(format!(
                "@property {{{}}} {}",
                m.fields.as_deref().unwrap_or("string"),
                m.name
            ));
        }
    }
    jsdoc(&mut out, "", &lines);

    out.push_str(
        "\n/**\n * A client for the API. Each method resolves to the response body.\n \
         * @param {ClientOptions} [options]\n */\n\
         export function createClient(options = {}) {\n  \
         const request = makeRequest(options);\n  return {\n",
    );
    for m in &methods {
        let mut lines: Vec<String> = m.doc.lines().map(String::from).collect();
        for (i, a) in m.args.iter().enumerate() {
            let (ty, name) = if optional_before_required(&m.args, i) {
                (format!("{} | undefined", a.ty), a.name.clone())
            } else if a.optional {
                (a.ty.clone(), format!("[{}]", a.name))
            } else {
                (a.ty.clone(), a.name.clone())
            };
            let text = a.doc.as_deref().map(one_line).unwrap_or_default();
            lines.push(
                format!("@param {{{ty}}} {name} {text}")
                    .trim_end()
                    .to_string(),
            );
        }
        lines.push("@param {CallOptions} [options]".into());
        lines.push(format!("@returns {{Promise<{}>}}", m.result));
        lines.extend(throws(m));
        jsdoc(&mut out, "    ", &lines);
        let mut args: Vec<&str> = m.args.iter().map(|a| a.name.as_str()).collect();
        args.push("options");
        let _ = writeln!(
            out,
            "    {}: ({}) =>\n      {},",
            m.name,
            args.join(", "),
            request_call(m, None)
        );
    }
    out.push_str(
        "  };\n}\n\n\
         /** @typedef {ReturnType<typeof createClient>} Client */\n\n\
         /** A same-origin client with the default options. */\n\
         export const api = createClient();\n",
    );
    Ok(Generated {
        source: out,
        operations: methods.len(),
        skipped,
        signatures: signatures(&methods),
    })
}

/// The text after `@property {T} name`: description and constraints on one
/// line (a JSDoc tag ends at the next `@`).
fn property_text(prop: &Value) -> String {
    let mut text = prop
        .get("description")
        .and_then(Value::as_str)
        .map(one_line)
        .unwrap_or_default();
    let mut notes: Vec<String> = facts(prop)
        .into_iter()
        .map(|(tag, v)| format!("{tag} {v}"))
        .collect();
    if prop.get("deprecated").and_then(Value::as_bool) == Some(true) {
        notes.insert(0, "deprecated".into());
    }
    if !notes.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        let _ = write!(text, "({})", notes.join(", "));
    }
    // `@` would start a new tag.
    text.replace('@', "(at)")
}

/// Writes a multi-line `/** */` comment.
fn jsdoc(out: &mut String, indent: &str, lines: &[String]) {
    let _ = writeln!(out, "{indent}/**");
    for line in lines {
        let line = escape_comment(line);
        for l in line.lines() {
            let _ = writeln!(out, "{indent} *{}{l}", if l.is_empty() { "" } else { " " });
        }
        if line.is_empty() {
            let _ = writeln!(out, "{indent} *");
        }
    }
    let _ = writeln!(out, "{indent} */");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> Value {
        json!({
            "openapi": "3.1.0",
            "info": {"title": "demo", "version": "1"},
            "paths": {
                "/users/{id}": {
                    "get": {"operationId": "get_user", "summary": "One user.",
                        "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}],
                        "responses": {"200": {"description": "", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}}},
                                      "404": {"description": "Not Found"}}},
                    "put": {"operationId": "replace",
                        "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}],
                        "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/User"}}}},
                        "responses": {"204": {"description": ""}, "422": {"description": "Validation Error"}}}
                },
                "/users": {"get": {"operationId": "list",
                    "parameters": [{"name": "limit", "in": "query", "schema": {"type": "integer"}},
                                   {"name": "params", "in": "query", "style": "form", "explode": true, "schema": {"type": "object"}}],
                    "responses": {"200": {"description": "", "content": {"application/json": {"schema": {"type": "array", "items": {"$ref": "#/components/schemas/User"}}}}}}}},
                "/": {"get": {"operationId": "page", "responses": {"200": {"description": "", "content": {"text/html": {}}}}}},
                "/files/{path}": {"get": {"operationId": "file",
                    "parameters": [{"name": "path", "in": "path", "required": true, "schema": {"type": "string"}, "x-zapline-catch-all": true}],
                    "responses": {"200": {"description": "", "content": {"text/plain": {}}}}}},
                "/upload/{default}": {"post": {"operationId": "upload",
                    "parameters": [{"name": "default", "in": "path", "required": true, "schema": {"type": "string"}},
                                   {"name": "tag", "in": "query", "required": true, "schema": {"type": "string"}}],
                    "requestBody": {"content": {"multipart/form-data": {"schema": {"type": "object"}}}},
                    "responses": {"200": {"description": "", "content": {"application/octet-stream": {}}}}}},
                "/events": {"get": {"operationId": "events", "responses": {"200": {"description": "", "content": {"text/event-stream": {}}}}}}
            },
            "components": {"schemas": {"User": {"type": "object", "description": "A user.", "required": ["id"], "properties": {"id": {"type": "integer"}, "name": {"type": ["string", "null"]}}}}}
        })
    }

    #[test]
    fn typescript_methods() {
        let g = typescript(&spec()).unwrap();
        assert_eq!(g.operations, 6);
        assert_eq!(g.skipped, vec!["GET /"]);
        let s = &g.source;
        assert!(s.contains("/** A user. */\nexport interface User {\n  id: number;\n  name?: string | null;\n}"), "{s}");
        assert!(s.contains("getUser: (id: number, options?: CallOptions) =>\n      request<User>(\"getUser\", \"GET\", `/users/${encodeURIComponent(String(id))}`, {}, options),"), "{s}");
        assert!(s.contains("replace: (id: number, body: User, options?: CallOptions) =>\n      request<void>(\"replace\", \"PUT\", `/users/${encodeURIComponent(String(id))}`, { json: body }, options),"), "{s}");
        assert!(s.contains("list: (query?: {\n      limit?: number;\n      [key: string]: unknown;\n    }, options?: CallOptions) =>"), "{s}");
        assert!(s.contains("file: (path: string, options?: CallOptions) =>\n      request<string>(\"file\", \"GET\", `/files/${String(path).split(\"/\").map(encodeURIComponent).join(\"/\")}`, { as: \"text\" }, options),"), "{s}");
        // An optional body before a required query; a keyword path param.
        assert!(s.contains("upload: (default_: string, body: FormData | Record<string, FormValue | FormValue[]> | undefined, query: {\n      tag: string;\n    }, options?: CallOptions) =>\n      request<Blob>(\"upload\", \"POST\", `/upload/${encodeURIComponent(String(default_))}`, { as: \"blob\", multipart: body, query }, options),"), "{s}");
        assert!(
            s.contains(
                "request<Response>(\"events\", \"GET\", `/events`, { as: \"response\" }, options)"
            ),
            "{s}"
        );
        assert!(
            s.contains("export interface ValidationFields {\n  replace: \"id\" | \"name\";\n}"),
            "{s}"
        );
        assert!(
            s.contains(
                "     * @throws {ValidationError<\"id\" | \"name\">} `422` Validation Error\n"
            ),
            "{s}"
        );
        assert!(s.contains("     * One user.\n     *\n     * `GET /users/{id}`\n     *\n     * @throws {ApiError} `404` Not Found\n     */"), "{s}");
    }

    #[test]
    fn javascript_methods() {
        let g = javascript(&spec()).unwrap();
        assert_eq!(g.operations, 6);
        let s = &g.source;
        assert!(s.contains("/**\n * A user.\n * @typedef {object} User\n * @property {number} id\n * @property {string | null} [name]\n */"), "{s}");
        assert!(s.contains("     * @param {number} id\n     * @param {CallOptions} [options]\n     * @returns {Promise<User>}\n"), "{s}");
        assert!(s.contains("    getUser: (id, options) =>\n      request(\"getUser\", \"GET\", `/users/${encodeURIComponent(String(id))}`, {}, options),"), "{s}");
        assert!(s.contains("     * @param {FormData | Record<string, FormValue | FormValue[]> | undefined} body\n     * @param {{ tag: string }} query\n"), "{s}");
        assert!(s.contains("/**\n * Field names a `422` validation error of each method can report.\n * @typedef {object} ValidationFields\n * @property {\"id\" | \"name\"} replace\n */"), "{s}");
        assert!(!s.contains("export interface"), "{s}");
    }

    #[test]
    fn changes() {
        let a = typescript(&spec()).unwrap();
        let mut spec2 = spec();
        spec2["paths"]["/users"]["get"]["parameters"][0]["schema"] = json!({"type": "string"});
        spec2["paths"].as_object_mut().unwrap().remove("/events");
        spec2["paths"]["/new"] =
            json!({"get": {"operationId": "fresh", "responses": {"204": {"description": ""}}}});
        let b = typescript(&spec2).unwrap();
        let c = b.changes_since(&a);
        assert_eq!(c.added, vec!["fresh"]);
        assert_eq!(c.removed, vec!["events"]);
        assert_eq!(c.changed, vec!["list"]);
        assert_eq!(c.to_string(), "+fresh -events ~list");
    }

    #[test]
    fn errors() {
        assert_eq!(
            typescript(&json!({})).unwrap_err().to_string(),
            "the OpenAPI document has no paths"
        );
    }
}
