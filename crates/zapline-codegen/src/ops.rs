//! OpenAPI operations to client methods: arguments, URL, body, result.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::{json, Map, Value};

use crate::names::{js_ident, method_name};
use crate::schema::Types;

/// Methods in the order they are emitted for each path.
const METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

/// One method of the generated client.
pub(crate) struct Method {
    pub name: String,
    pub http: String,
    /// Summary, description, `METHOD /path`, `@deprecated`.
    pub doc: String,
    pub args: Vec<Arg>,
    /// The URL as a JS template literal body.
    pub url: String,
    /// The `request()` init entries, e.g. `json: body`.
    pub init: Vec<String>,
    /// The resolved value's type.
    pub result: String,
    /// Field names a `422` validation error can report (a union of string
    /// literals, or `string`), when the operation documents a `422`.
    pub fields: Option<String>,
    /// Documented error responses: (status, description).
    pub errors: Vec<(String, String)>,
}

/// A method argument.
pub(crate) struct Arg {
    pub name: String,
    pub ty: String,
    pub optional: bool,
    pub doc: Option<String>,
}

/// Every operation of the document, plus `METHOD path` of the skipped ones
/// (HTML pages: not part of a JSON client).
pub(crate) fn methods(
    types: &Types<'_>,
    paths: &Map<String, Value>,
    indent: &str,
) -> (Vec<Method>, Vec<String>) {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut used = BTreeSet::new();
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        let shared = item.get("parameters");
        for method in METHODS {
            let Some(op) = item.get(*method) else {
                continue;
            };
            match operation(types, path, method, op, shared, &mut used, indent) {
                Some(m) => out.push(m),
                None => skipped.push(format!("{} {path}", method.to_uppercase())),
            }
        }
    }
    (out, skipped)
}

/// How the client reads a success response.
enum Reading {
    /// JSON (or text when the server answers with something else).
    Json(String),
    Text,
    Blob,
    /// Streams (SSE, NDJSON): the raw `Response`.
    Stream,
    Void,
}

fn is_json(media: &str) -> bool {
    let essence = media.split(';').next().unwrap_or("").trim();
    essence == "application/json" || essence.ends_with("+json")
}

fn success_reading(types: &Types<'_>, op: &Value, indent: &str) -> Option<Reading> {
    let responses = op.get("responses").and_then(Value::as_object);
    let success = responses.and_then(|r| {
        r.iter()
            .filter(|(code, _)| code.starts_with('2'))
            .min_by_key(|(code, _)| code.as_str())
            .map(|(_, v)| v)
    });
    let Some(content) = success
        .and_then(|s| s.get("content"))
        .and_then(Value::as_object)
        .filter(|c| !c.is_empty())
    else {
        return Some(Reading::Void);
    };
    if let Some((_, media)) = content.iter().find(|(k, _)| is_json(k)) {
        let ty = media
            .get("schema")
            .map_or("unknown".into(), |s| types.ts(s, indent));
        return Some(Reading::Json(ty));
    }
    let (media, _) = content.iter().next()?;
    let media = media.as_str();
    Some(if media.starts_with("text/html") {
        // An htmx page or fragment.
        return None;
    } else if media.starts_with("text/event-stream") || media.contains("ndjson") {
        Reading::Stream
    } else if media.starts_with("text/") {
        Reading::Text
    } else {
        Reading::Blob
    })
}

/// The request body.
struct Body<'s> {
    /// The `request()` init entries, e.g. `json: body`.
    init: String,
    /// The argument's type.
    ty: String,
    /// Its schema, for the validation field names.
    schema: Option<&'s Value>,
}

fn request_body<'s>(types: &Types<'_>, content: &'s Map<String, Value>, indent: &str) -> Body<'s> {
    let schema_of = |m: &'s Value| m.get("schema");
    if let Some((_, m)) = content.iter().find(|(k, _)| is_json(k)) {
        return Body {
            init: "json: body".into(),
            ty: schema_of(m).map_or("unknown".into(), |s| types.ts(s, indent)),
            schema: schema_of(m),
        };
    }
    if let Some(m) = content.get("application/x-www-form-urlencoded") {
        return Body {
            init: "form: body".into(),
            ty: schema_of(m).map_or("Record<string, QueryValue>".into(), |s| types.ts(s, indent)),
            schema: schema_of(m),
        };
    }
    if let Some(m) = content.get("multipart/form-data") {
        let schema = schema_of(m);
        let described = schema
            .map(|s| types.resolve(s))
            .and_then(|s| s.get("properties"))
            .and_then(Value::as_object)
            .is_some_and(|p| !p.is_empty());
        let ty = match schema {
            Some(s) if described => format!("FormData | {}", types.ts(s, indent)),
            _ => "FormData | Record<string, FormValue | FormValue[]>".into(),
        };
        return Body {
            init: "multipart: body".into(),
            ty,
            schema,
        };
    }
    let media = content
        .keys()
        .next()
        .map_or("application/octet-stream", String::as_str);
    let ty = if media.starts_with("text/") {
        "string"
    } else {
        "Blob | BufferSource | string"
    };
    Body {
        init: format!("raw: body, type: {}", Value::from(media)),
        ty: ty.into(),
        schema: None,
    }
}

fn operation(
    types: &Types<'_>,
    path: &str,
    method: &str,
    op: &Value,
    shared_params: Option<&Value>,
    used: &mut BTreeSet<String>,
    indent: &str,
) -> Option<Method> {
    let reading = success_reading(types, op, indent)?;

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
    let mut name = method_name(&id);
    while !used.insert(name.clone()) {
        name.push('_');
    }

    // Arguments: path params in path order, then body, then query, then
    // per-call options.
    let mut args = Vec::new();
    let mut idents: BTreeSet<String> = ["body", "query", "options"]
        .into_iter()
        .map(String::from)
        .collect();
    let mut url = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        url.push_str(&escape_template(&rest[..start]));
        let raw = &rest[start + 1..start + len];
        let param_name = raw.trim_start_matches('*');
        let param = path_params.iter().find(|p| {
            let n = p.get("name").and_then(Value::as_str);
            n == Some(raw) || n == Some(param_name)
        });
        let mut ident = js_ident(param_name);
        while !idents.insert(ident.clone()) {
            ident.push('_');
        }
        let ty = param
            .and_then(|p| p.get("schema"))
            .map_or("string".to_string(), |s| types.ts(s, indent));
        let catch_all = raw.starts_with('*')
            || param
                .and_then(|p| p.get("x-zapline-catch-all"))
                .and_then(Value::as_bool)
                == Some(true);
        args.push(Arg {
            name: ident.clone(),
            ty,
            optional: false,
            doc: param
                .and_then(|p| p.get("description"))
                .and_then(Value::as_str)
                .map(String::from),
        });
        if catch_all {
            // Keep the slashes of a `{*rest}` segment.
            let _ = write!(
                url,
                "${{String({ident}).split(\"/\").map(encodeURIComponent).join(\"/\")}}"
            );
        } else {
            let _ = write!(url, "${{encodeURIComponent(String({ident}))}}");
        }
        rest = &rest[start + len + 1..];
    }
    url.push_str(&escape_template(rest));

    let mut init = Vec::new();
    match &reading {
        Reading::Text => init.push("as: \"text\"".to_string()),
        Reading::Blob => init.push("as: \"blob\"".to_string()),
        Reading::Stream => init.push("as: \"response\"".to_string()),
        Reading::Json(_) | Reading::Void => {}
    }

    // Request body.
    let mut body_schema = None;
    if let Some(content) = op
        .pointer("/requestBody/content")
        .and_then(Value::as_object)
    {
        let body = request_body(types, content, indent);
        let required = op
            .pointer("/requestBody/required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        args.push(Arg {
            name: "body".into(),
            ty: body.ty,
            optional: !required,
            doc: op
                .pointer("/requestBody/description")
                .and_then(Value::as_str)
                .map(String::from),
        });
        init.push(body.init);
        body_schema = body.schema;
    }

    // Query: one object argument, typed like an object schema so its
    // members get doc comments.
    if !query_params.is_empty() {
        let mut props = Map::new();
        let mut required = Vec::new();
        let mut free_form = false;
        for p in &query_params {
            let pname = p.get("name").and_then(Value::as_str).unwrap_or("q");
            let mut schema = p.get("schema").cloned().unwrap_or_else(|| json!({}));
            let exploded_object = p.get("explode").and_then(Value::as_bool) != Some(false)
                && p.get("style").and_then(Value::as_str).unwrap_or("form") == "form"
                && types.resolve(&schema).get("type").and_then(Value::as_str) == Some("object")
                && !types
                    .resolve(&schema)
                    .get("properties")
                    .and_then(Value::as_object)
                    .is_some_and(|p| !p.is_empty());
            if exploded_object {
                // A free-form map: its keys are the query keys.
                free_form = true;
                continue;
            }
            if let (Some(obj), Some(d)) = (schema.as_object_mut(), p.get("description")) {
                obj.insert("description".into(), d.clone());
            }
            if let (Some(obj), Some(d)) = (schema.as_object_mut(), p.get("deprecated")) {
                obj.insert("deprecated".into(), d.clone());
            }
            if p.get("required").and_then(Value::as_bool) == Some(true) {
                required.push(Value::from(pname));
            }
            props.insert(pname.into(), schema);
        }
        let mut schema = json!({ "type": "object", "properties": props, "required": required });
        if free_form {
            schema["additionalProperties"] = Value::Bool(true);
        }
        let any_required = !required.is_empty();
        args.push(Arg {
            name: "query".into(),
            ty: types.object(&schema, indent),
            optional: !any_required,
            doc: None,
        });
        init.push("query".into());
    }

    let result = match reading {
        Reading::Json(t) => t,
        Reading::Text => "string".into(),
        Reading::Blob => "Blob".into(),
        Reading::Stream => "Response".into(),
        Reading::Void => "void".into(),
    };

    // Errors.
    let mut errors = Vec::new();
    let mut validates = false;
    if let Some(responses) = op.get("responses").and_then(Value::as_object) {
        for (code, r) in responses {
            if code.starts_with(['4', '5']) {
                let d = r.get("description").and_then(Value::as_str).unwrap_or("");
                errors.push((code.clone(), d.to_string()));
                validates |= code == "422";
            }
        }
    }
    let fields = validates.then(|| {
        let mut names = BTreeSet::new();
        if let Some(s) = body_schema {
            property_names(types, s, &mut names, 0);
        }
        if names.is_empty() {
            "string".into()
        } else {
            let names: Vec<String> = names.iter().map(|k| Value::from(*k).to_string()).collect();
            names.join(" | ")
        }
    });

    // Doc comment.
    let summary = op.get("summary").and_then(Value::as_str);
    let description = op.get("description").and_then(Value::as_str);
    let mut doc = format!("`{} {path}`", method.to_uppercase());
    if let Some(s) = summary {
        doc = format!("{s}\n\n{doc}");
    }
    if let Some(d) = description.filter(|d| Some(*d) != summary) {
        doc = format!("{doc}\n\n{d}");
    }
    if matches!(result.as_str(), "Response") {
        doc.push_str("\n\nResolves to the raw `Response`: read its body as a stream.");
    }
    if op.get("deprecated").and_then(Value::as_bool) == Some(true) {
        doc.push_str("\n\n@deprecated");
    }

    Some(Method {
        name,
        http: method.to_uppercase(),
        doc,
        args,
        url,
        init,
        result,
        fields,
        errors,
    })
}

/// Top-level property names of an object schema, or of every variant of a
/// `oneOf`/`anyOf`/`allOf`.
fn property_names<'a: 's, 's>(
    types: &Types<'a>,
    schema: &'s Value,
    out: &mut BTreeSet<&'s str>,
    depth: usize,
) {
    let schema = types.resolve(schema);
    if depth > 4 {
        return;
    }
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        out.extend(props.keys().map(String::as_str));
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        for part in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            property_names(types, part, out, depth + 1);
        }
    }
}

/// Text safe inside a JS template literal.
fn escape_template(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace("${", "\\${")
}

/// A one-line description of a doc string (JSDoc `@param` text).
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
