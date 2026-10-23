//! Building the OpenAPI document from the route table.

use std::collections::{BTreeMap, HashMap, HashSet};

use http::{Method, StatusCode};
use schemars::generate::{Contract, SchemaSettings};
use schemars::SchemaGenerator;
use serde_json::{json, Map, Value};

use super::doc::{
    AuthKind, BodyDoc, GroupDoc, OperationDoc, ParamIn, ResponseDoc, SchemaFn, Scheme, Status, Text,
};
use super::OpenApi;
use crate::auth::RoutePolicy;
use crate::RouteInfo;

/// Where response (and shared) schemas live.
const SCHEMAS: &str = "#/components/schemas/";
/// Temporary home of request-side schemas until they are merged.
const INPUT_DEFS: &str = "/components/x-zapline-input";
const INPUT: &str = "#/components/x-zapline-input/";

/// Two generators: request types are described as they *deserialize*,
/// response types as they *serialize* (they differ e.g. for `Option` fields
/// and serde defaults). Identical schemas are shared; differing request-side
/// ones get an `Input` suffix.
struct Schemas {
    input: SchemaGenerator,
    output: SchemaGenerator,
}

impl Schemas {
    fn new() -> Self {
        let settings = |contract: Contract, path: &'static str| {
            SchemaSettings::draft2020_12()
                .with(|s| {
                    s.definitions_path = path.into();
                    s.meta_schema = None;
                    s.contract = contract;
                })
                .into_generator()
        };
        let mut output = settings(Contract::Serialize, "/components/schemas");
        // Claim the `Error` name before any user type can.
        output.subschema_for::<crate::Error>();
        Schemas {
            input: settings(Contract::Deserialize, INPUT_DEFS),
            output,
        }
    }

    fn input(&mut self, f: SchemaFn) -> Value {
        let schema = f(&mut self.input).to_value();
        self.resolve_input_ref(schema)
    }

    fn output(&mut self, f: SchemaFn) -> Value {
        f(&mut self.output).to_value()
    }

    /// Follow a top-level `$ref` (e.g. a `#[serde(transparent)]` newtype) so
    /// parameter schemas can be inspected.
    fn resolve_input_ref(&self, schema: Value) -> Value {
        let target = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix(INPUT));
        match target {
            Some(name) => self
                .input
                .definitions()
                .iter()
                .find(|(k, _)| encode_ref_name(k) == name)
                .map(|(_, v)| v.clone())
                .unwrap_or(schema),
            None => schema,
        }
    }
}

/// The spec's security schemes: those declared on [`OpenApi`], plus
/// defaults added for kinds of credentials the app uses but didn't declare.
struct Schemes<'a> {
    declared: &'a [(String, Value)],
    added: Vec<(String, Value)>,
}

impl Schemes<'_> {
    fn all(&self) -> impl Iterator<Item = &(String, Value)> {
        self.declared.iter().chain(&self.added)
    }

    /// The name of the first scheme of this kind, adding a default one if
    /// there is none.
    fn resolve(&mut self, kind: AuthKind) -> String {
        if let Some((name, _)) = self.all().find(|(_, s)| is_kind(s, kind)) {
            return name.clone();
        }
        let (base, scheme) = match kind {
            AuthKind::Bearer => ("bearerAuth", json!({ "type": "http", "scheme": "bearer" })),
            AuthKind::Basic => ("basicAuth", json!({ "type": "http", "scheme": "basic" })),
            AuthKind::ApiKey => (
                "apiKeyAuth",
                json!({ "type": "apiKey", "in": "header", "name": "x-api-key" }),
            ),
        };
        let mut name = base.to_owned();
        let mut n = 2;
        while self.all().any(|(k, _)| *k == name) {
            name = format!("{base}{n}");
            n += 1;
        }
        self.added.push((name.clone(), scheme));
        name
    }

    /// OAuth-style schemes list scopes only; others may list role names.
    fn is_oauth(&self, name: &str) -> bool {
        self.all().any(|(k, s)| {
            k == name
                && matches!(
                    s.get("type").and_then(Value::as_str),
                    Some("oauth2" | "openIdConnect")
                )
        })
    }
}

fn is_kind(scheme: &Value, kind: AuthKind) -> bool {
    let field = |k: &str| scheme.get(k).and_then(Value::as_str);
    let http = |name: &str| {
        field("type") == Some("http")
            && field("scheme").is_some_and(|s| s.eq_ignore_ascii_case(name))
    };
    match kind {
        AuthKind::Bearer => http("bearer"),
        AuthKind::Basic => http("basic"),
        AuthKind::ApiKey => field("type") == Some("apiKey"),
    }
}

/// A documented route.
struct Op<'a> {
    path: String,
    params: Vec<String>,
    /// The `{*name}` parameter, which matches several segments.
    catch_all: Option<String>,
    method: &'static str,
    doc: &'a OperationDoc,
    group: Option<&'a GroupDoc>,
    policy: RoutePolicy<'a>,
}

pub(crate) fn generate(api: &OpenApi, routes: &[&'static RouteInfo]) -> Value {
    let empty = OperationDoc::default();
    let ops: Vec<Op<'_>> = routes
        .iter()
        .filter_map(|r| {
            let method = method_key(r.method()?)?;
            let doc = r.meta::<OperationDoc>().unwrap_or(&empty);
            #[cfg(feature = "fs")]
            let static_files = r.meta::<crate::fs::StaticFilesRoute>().is_some();
            #[cfg(not(feature = "fs"))]
            let static_files = false;
            if doc.hidden || static_files {
                return None;
            }
            let (path, params) = openapi_path(r.path());
            Some(Op {
                catch_all: catch_all_param(r.path()),
                path,
                params,
                method,
                doc,
                group: r.meta::<GroupDoc>(),
                policy: RoutePolicy::of(r),
            })
        })
        .collect();
    let ids = operation_ids(&ops);

    let mut schemas = Schemas::new();
    let mut schemes = Schemes {
        declared: &api.schemes,
        added: Vec::new(),
    };
    let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for (op, id) in ops.iter().zip(ids) {
        let mut value = operation(&mut schemas, op, id);
        if let Some(security) = security(&mut schemes, &api.security, op) {
            value.insert("security".into(), security);
        }
        paths
            .entry(op.path.clone())
            .or_default()
            .insert(op.method.to_owned(), Value::Object(value));
    }
    let mut paths = Value::Object(
        paths
            .into_iter()
            .map(|(k, v)| (k, Value::Object(v)))
            .collect(),
    );
    let mut components = merge_schemas(schemas, &mut paths);

    let mut spec = Map::new();
    spec.insert("openapi".into(), "3.1.0".into());
    spec.insert("info".into(), info(api));
    if !api.servers.is_empty() {
        let servers = api.servers.iter().map(|(url, description)| {
            let mut s = Map::new();
            s.insert("url".into(), url.clone().into());
            if let Some(d) = description {
                s.insert("description".into(), d.clone().into());
            }
            Value::Object(s)
        });
        spec.insert("servers".into(), servers.collect());
    }
    if !api.tags.is_empty() {
        let tags = api.tags.iter().map(|(name, description)| {
            let mut t = Map::new();
            t.insert("name".into(), name.clone().into());
            if let Some(d) = description {
                t.insert("description".into(), d.clone().into());
            }
            Value::Object(t)
        });
        spec.insert("tags".into(), tags.collect());
    }
    spec.insert("paths".into(), paths);
    let schemes: Map<String, Value> = schemes.all().cloned().collect();
    if !schemes.is_empty() {
        components.insert("securitySchemes".into(), Value::Object(schemes));
    }
    spec.insert("components".into(), Value::Object(components));
    if !api.security.is_empty() {
        spec.insert("security".into(), security_list(&api.security));
    }
    let mut spec = Value::Object(spec);
    for hook in &api.hooks {
        hook(&mut spec);
    }
    spec
}

fn info(api: &OpenApi) -> Value {
    let mut info = Map::new();
    info.insert("title".into(), api.title.clone().into());
    info.insert("version".into(), api.version.clone().into());
    if let Some(d) = &api.description {
        info.insert("description".into(), d.clone().into());
    }
    if let Some((name, email)) = &api.contact {
        info.insert("contact".into(), json!({ "name": name, "email": email }));
    }
    if let Some(license) = &api.license {
        info.insert("license".into(), json!({ "name": license }));
    }
    Value::Object(info)
}

/// The OpenAPI field name for a method; `None` for methods OpenAPI 3.1
/// cannot express.
fn method_key(m: &Method) -> Option<&'static str> {
    Some(match *m {
        Method::GET => "get",
        Method::POST => "post",
        Method::PUT => "put",
        Method::PATCH => "patch",
        Method::DELETE => "delete",
        Method::HEAD => "head",
        Method::OPTIONS => "options",
        Method::TRACE => "trace",
        _ => return None,
    })
}

/// `/users/{id}/{*rest}` -> (`/users/{id}/{rest}`, `["id", "rest"]`).
pub(crate) fn openapi_path(pattern: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(pattern.len());
    let mut names = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let name = name.trim_start_matches('*').to_owned();
                out.push('{');
                out.push_str(&name);
                out.push('}');
                names.push(name);
            }
            c => out.push(c),
        }
    }
    (out, names)
}

/// The name of a route's `{*name}` parameter, if it has one.
fn catch_all_param(pattern: &str) -> Option<String> {
    let start = pattern.find("{*")? + 2;
    let len = pattern[start..].find('}')?;
    Some(pattern[start..start + len].to_owned())
}

/// `GET /users/{id}` -> `get_users_id`.
fn default_operation_id(method: &str, path: &str) -> String {
    let mut slug = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('_') {
            slug.push('_');
        }
    }
    let slug = slug.trim_end_matches('_');
    if slug.is_empty() {
        format!("{method}_root")
    } else {
        format!("{method}_{slug}")
    }
}

/// Unique operation ids. Explicit ids are reserved first. Same-named
/// handlers from different modules are prefixed with their module
/// (`users::list` and `posts::list` become `users_list` and `posts_list`);
/// any remaining clash gets the method appended, then a counter.
fn operation_ids(ops: &[Op<'_>]) -> Vec<String> {
    // Handler name -> the modules it is defined in.
    let mut modules: std::collections::HashMap<&str, HashSet<&str>> = Default::default();
    for op in ops.iter().filter(|op| op.doc.operation_id.is_none()) {
        if let (Some(h), Some(m)) = (op.doc.handler, op.doc.module) {
            modules.entry(h).or_default().insert(m);
        }
    }
    let mut used = HashSet::new();
    let mut ids: Vec<Option<String>> = vec![None; ops.len()];
    let claim = |base: String, method: &str, used: &mut HashSet<String>| {
        let mut id = base.clone();
        if used.contains(&id) {
            id = format!("{base}_{method}");
        }
        let mut n = 2;
        while used.contains(&id) {
            id = format!("{base}_{n}");
            n += 1;
        }
        used.insert(id.clone());
        id
    };
    for (i, op) in ops.iter().enumerate() {
        if let Some(id) = &op.doc.operation_id {
            ids[i] = Some(claim(id.to_string(), op.method, &mut used));
        }
    }
    for (i, op) in ops.iter().enumerate() {
        if ids[i].is_none() {
            let base = match (op.doc.handler, op.doc.module) {
                (Some(h), Some(m)) if modules.get(h).is_some_and(|ms| ms.len() > 1) => {
                    let last = m.rsplit("::").next().unwrap_or(m);
                    format!("{last}_{h}")
                }
                (Some(h), _) => h.to_owned(),
                (None, _) => default_operation_id(op.method, &op.path),
            };
            ids[i] = Some(claim(base, op.method, &mut used));
        }
    }
    ids.into_iter().map(Option::unwrap_or_default).collect()
}

fn operation(s: &mut Schemas, op: &Op<'_>, id: String) -> Map<String, Value> {
    let doc = op.doc;
    let mut o = Map::new();

    let mut tags: Vec<&Text> = Vec::new();
    for t in op
        .group
        .map(|g| &g.tags[..])
        .unwrap_or(&[])
        .iter()
        .chain(&doc.tags)
    {
        if !tags.contains(&t) {
            tags.push(t);
        }
    }
    if !tags.is_empty() {
        o.insert("tags".into(), tags.iter().map(|t| t.to_string()).collect());
    }
    if let Some(v) = &doc.summary {
        o.insert("summary".into(), v.to_string().into());
    }
    if let Some(v) = &doc.description {
        o.insert("description".into(), v.to_string().into());
    }
    o.insert("operationId".into(), id.into());

    let params = parameters(s, op);
    if !params.is_empty() {
        o.insert("parameters".into(), Value::Array(params));
    }
    if let Some(body) = &doc.body {
        o.insert("requestBody".into(), request_body(s, body));
    }
    // What the route's auth policy answers with (documented ones win).
    let mut policy = Vec::new();
    if op.policy.requires_auth() {
        policy.push(ResponseDoc::error(401, "Unauthorized"));
    }
    if op.policy.has_permissions() {
        policy.push(ResponseDoc::error(403, "Forbidden"));
    }
    o.insert("responses".into(), responses(s, doc, &policy));
    if doc.deprecated {
        o.insert("deprecated".into(), true.into());
    }
    o
}

/// An operation's security requirements, or `None` to leave the spec-wide
/// default in effect. In order of precedence:
///
/// 1. its own schemes (explicit, or from credential extractors), plus `{}`
///    when they all come from optional extractors;
/// 2. `[]` for `.allow_anonymous()` routes;
/// 3. its group's schemes (`security_all`);
/// 4. for routes with an auth policy, the spec's default security, else the
///    app's bearer scheme.
///
/// Required scopes are listed in each requirement; required roles (any one
/// suffices) make one alternative per role, except for OAuth schemes.
fn security(schemes: &mut Schemes<'_>, global: &[String], op: &Op<'_>) -> Option<Value> {
    let group = op.group.map_or(&[][..], |g| &g.security[..]);
    // How the app authenticates users: group, spec default, else bearer.
    let user = |schemes: &mut Schemes<'_>| -> Vec<String> {
        if !group.is_empty() {
            group.iter().map(ToString::to_string).collect()
        } else if !global.is_empty() {
            global.to_vec()
        } else {
            vec![schemes.resolve(AuthKind::Bearer)]
        }
    };

    let own = &op.doc.security;
    let mut names: Vec<String> = Vec::new();
    let mut anonymous = false;
    if !own.is_empty() {
        for sec in own {
            let resolved = match &sec.scheme {
                Scheme::Named(name) => vec![name.to_string()],
                Scheme::Kind(kind) => vec![schemes.resolve(*kind)],
                Scheme::User => user(schemes),
            };
            for name in resolved {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        anonymous = !op.policy.requires_auth() && own.iter().all(|s| s.optional);
    } else if op.policy.is_anonymous() {
        return Some(Value::Array(Vec::new()));
    } else if !group.is_empty() {
        names = group.iter().map(ToString::to_string).collect();
    } else if op.policy.requires_auth() {
        names = user(schemes);
    } else {
        return None;
    }

    let scopes = op.policy.scopes();
    let mut requirements = Vec::new();
    for name in &names {
        let roles = if schemes.is_oauth(name) {
            &[][..]
        } else {
            op.policy.roles()
        };
        if roles.is_empty() {
            requirements.push(requirement(name, scopes.iter()));
        }
        for role in roles {
            requirements.push(requirement(name, std::iter::once(role).chain(scopes)));
        }
    }
    if anonymous {
        requirements.push(Value::Object(Map::new()));
    }
    Some(Value::Array(requirements))
}

/// `{ "<scheme>": [<scopes or roles>] }`, without duplicates.
fn requirement<'a>(scheme: &str, values: impl Iterator<Item = &'a String>) -> Value {
    let mut list: Vec<Value> = Vec::new();
    for v in values {
        let v = Value::from(v.as_str());
        if !list.contains(&v) {
            list.push(v);
        }
    }
    let mut m = Map::new();
    m.insert(scheme.to_owned(), Value::Array(list));
    Value::Object(m)
}

/// Alternatives: any one of the schemes satisfies the requirement.
fn security_list<S: AsRef<str>>(schemes: &[S]) -> Value {
    schemes
        .iter()
        .map(|s| {
            let mut m = Map::new();
            m.insert(s.as_ref().to_owned(), Value::Array(Vec::new()));
            Value::Object(m)
        })
        .collect()
}

fn param(name: &str, location: ParamIn, required: bool, schema: Value) -> Value {
    let (schema, description) = split_description(schema);
    let mut p = Map::new();
    p.insert("name".into(), name.into());
    p.insert("in".into(), location.as_str().into());
    if let Some(d) = description {
        p.insert("description".into(), d);
    }
    p.insert("required".into(), required.into());
    if schema.get("deprecated") == Some(&Value::Bool(true)) {
        p.insert("deprecated".into(), true.into());
    }
    p.insert("schema".into(), non_null(schema));
    Value::Object(p)
}

fn parameters(s: &mut Schemas, op: &Op<'_>) -> Vec<Value> {
    let doc = op.doc;
    let mut out = Vec::new();
    let mut seen: HashSet<(String, ParamIn)> = HashSet::new();

    // Path parameters: always all of them, typed by `Path<T>` or explicit docs.
    let typed = match doc.path {
        Some(f) => path_schemas(&s.input(f), &op.params),
        None => vec![None; op.params.len()],
    };
    for (name, typed) in op.params.iter().zip(typed) {
        let explicit = doc
            .params
            .iter()
            .find(|p| p.location == ParamIn::Path && p.name == name.as_str());
        let schema = match (explicit, typed) {
            (Some(p), _) => s.input(p.schema),
            (None, Some(t)) => t,
            (None, None) => json!({ "type": "string" }),
        };
        let mut p = param(name, ParamIn::Path, true, schema);
        if op.catch_all.as_ref() == Some(name) {
            // Clients must keep its slashes: `zapline client` reads this.
            p["x-zapline-catch-all"] = true.into();
        }
        out.push(p);
        seen.insert((name.clone(), ParamIn::Path));
    }

    for p in doc.params.iter().filter(|p| p.location != ParamIn::Path) {
        if seen.insert((p.name.to_string(), p.location)) {
            let schema = s.input(p.schema);
            out.push(param(&p.name, p.location, p.required, schema));
        }
    }

    for q in &doc.query {
        let schema = s.input(q.schema);
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            let required: HashSet<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for (name, prop) in props {
                if seen.insert((name.clone(), ParamIn::Query)) {
                    let req = !q.optional && required.contains(name.as_str());
                    out.push(param(name, ParamIn::Query, req, prop.clone()));
                }
            }
        } else if schema.get("type") == Some(&json!("object")) {
            // A free-form map (e.g. `HashMap<String, String>`).
            let mut p = Map::new();
            p.insert("name".into(), "params".into());
            p.insert("in".into(), "query".into());
            p.insert("style".into(), "form".into());
            p.insert("explode".into(), true.into());
            p.insert("schema".into(), schema);
            out.push(Value::Object(p));
        }
    }
    out
}

/// Schemas for each route parameter from `Path<T>`'s schema: struct fields
/// by name, tuple items by position, or a scalar for a single parameter.
fn path_schemas(schema: &Value, names: &[String]) -> Vec<Option<Value>> {
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        return names.iter().map(|n| props.get(n).cloned()).collect();
    }
    let items = schema
        .get("prefixItems")
        .or_else(|| schema.get("items"))
        .and_then(Value::as_array);
    if let Some(items) = items {
        return (0..names.len()).map(|i| items.get(i).cloned()).collect();
    }
    let compound = matches!(
        schema.get("type").and_then(Value::as_str),
        Some("object" | "array")
    );
    if names.len() == 1 && !compound {
        return vec![Some(schema.clone())];
    }
    vec![None; names.len()]
}

/// Move a schema's `description` to the parameter.
fn split_description(mut schema: Value) -> (Value, Option<Value>) {
    let description = schema.as_object_mut().and_then(|o| o.remove("description"));
    (schema, description)
}

/// Parameters are never `null`: drop `null` from `Option<T>` schemas.
fn non_null(mut schema: Value) -> Value {
    let Some(obj) = schema.as_object_mut() else {
        return schema;
    };
    if let Some(Value::Array(types)) = obj.get_mut("type") {
        types.retain(|t| t != "null");
        if types.len() == 1 {
            let t = types.remove(0);
            obj.insert("type".into(), t);
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(alts)) = obj.get_mut(key) {
            alts.retain(|a| a != &json!({ "type": "null" }));
            if alts.len() == 1 {
                let only = alts.remove(0);
                obj.remove(key);
                if obj.is_empty() {
                    return only;
                }
                obj.insert("allOf".into(), Value::Array(vec![only]));
            }
        }
    }
    schema
}

fn media(schema: Option<Value>) -> Value {
    match schema {
        Some(schema) => json!({ "schema": schema }),
        None => Value::Object(Map::new()),
    }
}

fn request_body(s: &mut Schemas, body: &BodyDoc) -> Value {
    let mut content = Map::new();
    for (ct, schema) in &body.content {
        let schema = schema.map(|f| f(&mut s.input).to_value());
        content.insert(ct.to_string(), media(schema));
    }
    json!({ "required": body.required, "content": content })
}

fn reason(code: Option<u16>) -> &'static str {
    match code {
        None => "Error",
        Some(c) => StatusCode::from_u16(c)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("Response"),
    }
}

fn responses(s: &mut Schemas, doc: &OperationDoc, extra: &[ResponseDoc]) -> Value {
    struct Acc {
        code: Option<u16>,
        description: Option<Text>,
        content: Map<String, Value>,
        headers: Map<String, Value>,
    }
    let success = doc.success_status.unwrap_or(200);
    let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
    let mut has_success = false;
    for r in doc.responses.iter().chain(extra) {
        let code = match r.status {
            Status::Success => Some(success),
            Status::Code(c) => Some(c),
            Status::Default => None,
        };
        has_success |= code.is_some_and(|c| c < 400);
        let key = code.map_or_else(|| "default".to_owned(), |c| c.to_string());
        let entry = acc.entry(key).or_insert_with(|| Acc {
            code,
            description: None,
            content: Map::new(),
            headers: Map::new(),
        });
        if entry.description.is_none() {
            entry.description.clone_from(&r.description);
        }
        if let Some((ct, schema)) = &r.content {
            if !entry.content.contains_key(ct.as_ref()) {
                let schema = schema.map(|f| s.output(f));
                entry.content.insert(ct.to_string(), media(schema));
            }
        }
        for (name, description) in &r.headers {
            entry.headers.insert(
                name.to_string(),
                json!({ "description": description, "schema": { "type": "string" } }),
            );
        }
    }
    if !has_success {
        acc.insert(
            success.to_string(),
            Acc {
                code: Some(success),
                description: None,
                content: Map::new(),
                headers: Map::new(),
            },
        );
    }
    let out: Map<String, Value> = acc
        .into_iter()
        .map(|(key, a)| {
            let mut r = Map::new();
            let description = a
                .description
                .map_or_else(|| reason(a.code).to_owned(), |d| d.to_string());
            r.insert("description".into(), description.into());
            if !a.headers.is_empty() {
                r.insert("headers".into(), Value::Object(a.headers));
            }
            if !a.content.is_empty() {
                r.insert("content".into(), Value::Object(a.content));
            }
            (key, Value::Object(r))
        })
        .collect();
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Merging request- and response-side component schemas

/// Merge both generators' definitions into `components.schemas` and point
/// every request-side `$ref` (in `paths` and in the schemas) at its final
/// name. Returns the `components` object.
fn merge_schemas(s: Schemas, paths: &mut Value) -> Map<String, Value> {
    let Schemas {
        mut input,
        mut output,
    } = s;
    let out_defs = output.take_definitions(true);
    let in_defs = input.take_definitions(true);

    let by_encoded: HashMap<String, &String> =
        in_defs.keys().map(|k| (encode_ref_name(k), k)).collect();
    let input_refs = |v: &Value| -> Vec<&String> {
        let mut refs = Vec::new();
        visit_refs(v, &mut |r| {
            if let Some(k) = r.strip_prefix(INPUT).and_then(|n| by_encoded.get(n)) {
                refs.push(*k);
            }
        });
        refs
    };

    // Request-side schemas that differ from the response-side schema of the
    // same name (directly, or through a schema they reference).
    let mut differs: HashSet<&String> = in_defs
        .iter()
        .filter(|(k, v)| {
            out_defs.get(*k).is_some_and(|o| {
                let mut normalized = (*v).clone();
                rewrite_refs(&mut normalized, &mut |r| {
                    if let Some(n) = r.strip_prefix(INPUT) {
                        *r = format!("{SCHEMAS}{n}");
                    }
                });
                *o != normalized
            })
        })
        .map(|(k, _)| k)
        .collect();
    loop {
        let before = differs.len();
        for (k, v) in &in_defs {
            if out_defs.contains_key(k)
                && !differs.contains(k)
                && input_refs(v).iter().any(|r| differs.contains(r))
            {
                differs.insert(k);
            }
        }
        if differs.len() == before {
            break;
        }
    }

    let mut taken: HashSet<String> = out_defs.keys().cloned().collect();
    taken.extend(
        in_defs
            .keys()
            .filter(|k| !out_defs.contains_key(*k))
            .cloned(),
    );
    let mut final_names: HashMap<&String, String> = HashMap::new();
    for k in in_defs.keys() {
        let mut name = k.clone();
        if differs.contains(k) {
            name = format!("{k}Input");
            let mut n = 2;
            while taken.contains(&name) {
                name = format!("{k}Input{n}");
                n += 1;
            }
            taken.insert(name.clone());
        }
        final_names.insert(k, name);
    }
    let renames: HashMap<String, String> = final_names
        .iter()
        .map(|(k, name)| {
            (
                format!("{INPUT}{}", encode_ref_name(k)),
                format!("{SCHEMAS}{}", encode_ref_name(name)),
            )
        })
        .collect();

    let mut schemas = out_defs;
    for (k, v) in &in_defs {
        let name = &final_names[k];
        if differs.contains(k) || !schemas.contains_key(name) {
            schemas.insert(name.clone(), v.clone());
        }
    }

    let mut fix = |r: &mut String| {
        if let Some(new) = renames.get(r.as_str()) {
            r.clone_from(new);
        } else if let Some(n) = r.strip_prefix(INPUT) {
            *r = format!("{SCHEMAS}{n}");
        }
    };
    for v in schemas.values_mut() {
        rewrite_refs(v, &mut fix);
    }
    rewrite_refs(paths, &mut fix);

    let mut components = Map::new();
    components.insert("schemas".into(), Value::Object(schemas));
    components
}

fn visit_refs<'a>(v: &'a Value, f: &mut impl FnMut(&'a str)) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x) {
                    ("$ref", Value::String(r)) => f(r),
                    _ => visit_refs(x, f),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| visit_refs(x, f)),
        _ => {}
    }
}

fn rewrite_refs(v: &mut Value, f: &mut impl FnMut(&mut String)) {
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                match x {
                    Value::String(r) if k == "$ref" => f(r),
                    _ => rewrite_refs(x, f),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| rewrite_refs(x, f)),
        _ => {}
    }
}

/// Same encoding schemars uses for definition names inside `$ref`s.
fn encode_ref_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'~' => out.push_str("~0"),
            b'/' => out.push_str("~1"),
            b'!' | b'$' | b'&'..=b';' | b'=' | b'?'..=b'Z' | b'_' | b'a'..=b'z' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:2X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Serialization in the conventional key order

/// Keys listed here come first, in this order; others keep their natural
/// order. Purely cosmetic: `openapi` and `info` lead the document, and
/// `summary`/`operationId` lead each operation.
const KEY_ORDER: &[&str] = &[
    "openapi",
    "info",
    "jsonSchemaDialect",
    "servers",
    "tags",
    "title",
    "name",
    "url",
    "in",
    "summary",
    "description",
    "operationId",
    "version",
    "termsOfService",
    "contact",
    "email",
    "license",
    "required",
    "deprecated",
    "style",
    "explode",
    "parameters",
    "requestBody",
    "responses",
    "headers",
    "content",
    "schema",
    "security",
    "paths",
    "webhooks",
    "components",
];

/// Maps whose keys are names chosen by users (paths, properties...): their
/// own keys are never reordered.
const NAMED_MAPS: &[&str] = &[
    "paths",
    "properties",
    "schemas",
    "responses",
    "content",
    "headers",
    "securitySchemes",
    "patternProperties",
    "$defs",
    "webhooks",
    "examples",
];

struct Ordered<'a> {
    value: &'a Value,
    reorder: bool,
}

impl serde::Serialize for Ordered<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self.value {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                if self.reorder {
                    let rank =
                        |k: &str| KEY_ORDER.iter().position(|o| *o == k).unwrap_or(usize::MAX);
                    keys.sort_by_key(|k| rank(k));
                }
                let mut map = s.serialize_map(Some(keys.len()))?;
                for k in keys {
                    let child = &m[k.as_str()];
                    // Under a named map, children are named entries: keep
                    // their keys; inside each entry, reorder again.
                    let entry = Ordered {
                        value: child,
                        reorder: !(self.reorder && NAMED_MAPS.contains(&k.as_str())),
                    };
                    map.serialize_entry(k, &entry)?;
                }
                map.end()
            }
            Value::Array(a) => s.collect_seq(a.iter().map(|value| Ordered {
                value,
                reorder: true,
            })),
            v => v.serialize(s),
        }
    }
}

/// The spec as JSON bytes in the conventional key order.
pub(crate) fn to_json(spec: &Value, pretty: bool) -> Vec<u8> {
    let ordered = Ordered {
        value: spec,
        reorder: true,
    };
    let bytes = if pretty {
        serde_json::to_vec_pretty(&ordered)
    } else {
        serde_json::to_vec(&ordered)
    };
    bytes.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_paths() {
        assert_eq!(
            openapi_path("/users/{id}/files/{*rest}"),
            (
                "/users/{id}/files/{rest}".to_owned(),
                vec!["id".into(), "rest".into()]
            )
        );
        assert_eq!(openapi_path("/a{{b}}"), ("/a{b}".to_owned(), vec![]));
    }

    #[test]
    fn default_ids() {
        assert_eq!(default_operation_id("get", "/users/{id}"), "get_users_id");
        assert_eq!(default_operation_id("post", "/"), "post_root");
        assert_eq!(
            default_operation_id("get", "/a-b//c.json"),
            "get_a_b_c_json"
        );
    }

    #[test]
    fn strips_null_from_params() {
        assert_eq!(
            non_null(json!({ "type": ["integer", "null"], "format": "int32" })),
            json!({ "type": "integer", "format": "int32" })
        );
        assert_eq!(
            non_null(json!({ "anyOf": [{ "$ref": "#/x" }, { "type": "null" }] })),
            json!({ "$ref": "#/x" })
        );
    }

    #[test]
    fn orders_keys_conventionally() {
        let spec = json!({
            "paths": { "/b": {}, "/a": { "get": { "responses": {}, "summary": "s" } } },
            "info": { "version": "1", "title": "t" },
            "openapi": "3.1.0",
            "components": { "schemas": { "description": { "type": "object" } } }
        });
        let text = String::from_utf8(to_json(&spec, false)).unwrap();
        assert_eq!(
            text,
            r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{"/a":{"get":{"summary":"s","responses":{}}},"/b":{}},"components":{"schemas":{"description":{"type":"object"}}}}"#
        );
    }

    #[test]
    fn encodes_like_schemars() {
        assert_eq!(encode_ref_name("Page_for_User"), "Page_for_User");
        assert_eq!(encode_ref_name("a/b~c d"), "a~1b~0c%20d");
    }
}
