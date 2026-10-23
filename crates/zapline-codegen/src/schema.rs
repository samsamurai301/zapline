//! JSON Schema (OpenAPI 3.1, plus 3.0's `nullable`) to TypeScript types.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde_json::{Map, Value};

use crate::names::{prop_name, type_name};

const SCHEMAS: &str = "#/components/schemas/";

/// The schemas of a document and how to name and render them.
pub(crate) struct Types<'a> {
    schemas: &'a Map<String, Value>,
    /// Schema key -> unique TypeScript name.
    names: BTreeMap<&'a str, String>,
    /// Multi-line object types with doc comments (TypeScript), or
    /// single-line ones (JSDoc).
    multiline: bool,
}

impl<'a> Types<'a> {
    pub(crate) fn new(schemas: &'a Map<String, Value>, multiline: bool) -> Self {
        let mut names = BTreeMap::new();
        let mut used = BTreeSet::new();
        for key in schemas.keys() {
            let base = type_name(key);
            let mut name = base.clone();
            let mut n = 2;
            while !used.insert(name.clone()) {
                name = format!("{base}{n}");
                n += 1;
            }
            names.insert(key.as_str(), name);
        }
        Types {
            schemas,
            names,
            multiline,
        }
    }

    /// Named schemas in document order: (key, name, schema).
    pub(crate) fn named(&self) -> impl Iterator<Item = (&'a str, &str, &'a Value)> + '_ {
        self.schemas
            .iter()
            .map(|(k, v)| (k.as_str(), self.names[k.as_str()].as_str(), v))
    }

    fn by_ref(&self, r: &str) -> Option<(&str, &'a Value)> {
        let key = r.strip_prefix(SCHEMAS)?;
        let (k, v) = self.schemas.get_key_value(key)?;
        Some((self.names[k.as_str()].as_str(), v))
    }

    /// Follows `$ref`s (a few levels) to the schema they name.
    pub(crate) fn resolve<'s>(&self, mut schema: &'s Value) -> &'s Value
    where
        'a: 's,
    {
        for _ in 0..8 {
            match schema.get("$ref").and_then(Value::as_str) {
                Some(r) => match self.by_ref(r) {
                    Some((_, s)) => schema = s,
                    None => break,
                },
                None => break,
            }
        }
        schema
    }

    /// The TypeScript type of `schema`; `indent` is where nested object
    /// members start, in multi-line mode.
    pub(crate) fn ts(&self, schema: &Value, indent: &str) -> String {
        let Some(obj) = schema.as_object() else {
            // `true` accepts anything, `false` nothing.
            return if schema == &Value::Bool(false) {
                "never".into()
            } else {
                "unknown".into()
            };
        };
        let ty = self.ts_inner(obj, schema, indent);
        if obj.get("nullable").and_then(Value::as_bool) == Some(true) {
            union(vec![wrap(ty), "null".into()])
        } else {
            ty
        }
    }

    fn ts_inner(&self, obj: &Map<String, Value>, schema: &Value, indent: &str) -> String {
        if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
            return match self.by_ref(r) {
                Some((name, _)) => name.to_string(),
                None => "unknown".into(),
            };
        }
        if let Some(c) = obj.get("const") {
            return c.to_string();
        }
        if let Some(values) = obj.get("enum").and_then(Value::as_array) {
            return union(values.iter().map(Value::to_string).collect());
        }
        for (key, sep) in [("oneOf", " | "), ("anyOf", " | "), ("allOf", " & ")] {
            let Some(parts) = obj.get(key).and_then(Value::as_array) else {
                continue;
            };
            let siblings = has_properties(obj);
            if let ([only], false) = (parts.as_slice(), siblings) {
                return self.ts(only, indent);
            }
            let mut types: Vec<String> = parts.iter().map(|p| wrap(self.ts(p, indent))).collect();
            // Sibling properties (an object with a discriminator, say).
            if siblings {
                types.push(self.object(schema, indent));
            }
            return if sep == " | " {
                union(types)
            } else {
                types.join(sep)
            };
        }
        let types: Vec<&str> = match obj.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ if obj.contains_key("properties") => vec!["object"],
            _ if obj.contains_key("items") || obj.contains_key("prefixItems") => vec!["array"],
            _ => return "unknown".into(),
        };
        let ts: Vec<String> = types
            .iter()
            .map(|t| match *t {
                "string" if obj.get("format").and_then(Value::as_str) == Some("binary") => {
                    "Blob".to_string()
                }
                "string" => "string".to_string(),
                "integer" | "number" => "number".to_string(),
                "boolean" => "boolean".to_string(),
                "null" => "null".to_string(),
                "array" => self.array(obj, indent),
                "object" if has_properties(obj) => self.object(schema, indent),
                "object" => match obj.get("additionalProperties") {
                    Some(Value::Bool(false)) => "Record<string, never>".to_string(),
                    Some(extra @ Value::Object(_)) => {
                        format!("Record<string, {}>", self.ts(extra, indent))
                    }
                    _ => "Record<string, unknown>".to_string(),
                },
                _ => "unknown".to_string(),
            })
            .collect();
        union(
            ts.into_iter()
                .map(|t| if types.len() > 1 { wrap(t) } else { t })
                .collect(),
        )
    }

    fn array(&self, obj: &Map<String, Value>, indent: &str) -> String {
        if let Some(items) = obj.get("prefixItems").and_then(Value::as_array) {
            let parts: Vec<String> = items.iter().map(|i| self.ts(i, indent)).collect();
            return format!("[{}]", parts.join(", "));
        }
        match obj.get("items") {
            Some(items) => format!("{}[]", wrap(self.ts(items, indent))),
            None => "unknown[]".into(),
        }
    }

    /// An object type literal: `{ a: string; b?: number }`, multi-line with
    /// doc comments in multi-line mode.
    pub(crate) fn object(&self, schema: &Value, indent: &str) -> String {
        let required: BTreeSet<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let inner = format!("{indent}  ");
        // (doc comment, member)
        let mut members: Vec<(Option<String>, String)> = Vec::new();
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (name, prop) in props {
                let member = format!(
                    "{}{}: {}",
                    prop_name(name),
                    if required.contains(name.as_str()) {
                        ""
                    } else {
                        "?"
                    },
                    self.ts(prop, &inner)
                );
                members.push((prop_doc(prop), member));
            }
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(true)) => members.push((None, "[key: string]: unknown".into())),
            Some(extra @ Value::Object(_)) => {
                members.push((None, format!("[key: string]: {}", self.ts(extra, &inner))));
            }
            _ => {}
        }
        if members.is_empty() {
            return "{}".into();
        }
        if !self.multiline {
            let members: Vec<String> = members.into_iter().map(|(_, m)| m).collect();
            return format!("{{ {} }}", members.join("; "));
        }
        let mut out = String::from("{\n");
        for (doc, member) in members {
            doc_comment(&mut out, &inner, doc.as_deref());
            let _ = writeln!(out, "{inner}{member};");
        }
        out.push_str(indent);
        out.push('}');
        out
    }
}

fn has_properties(obj: &Map<String, Value>) -> bool {
    obj.get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty())
}

/// Whether a named schema is best declared as an `interface`.
pub(crate) fn is_interface(schema: &Value) -> bool {
    schema
        .as_object()
        .is_some_and(|o| has_properties(o) && !o.contains_key("nullable"))
        && ["oneOf", "anyOf", "allOf", "$ref", "enum", "const"]
            .iter()
            .all(|k| schema.get(k).is_none())
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
pub(crate) fn wrap(t: String) -> String {
    let bare_union = (t.contains(" | ") || t.contains(" & ")) && !t.starts_with('{');
    if bare_union && !(t.starts_with('(') && t.ends_with(')')) {
        format!("({t})")
    } else {
        t
    }
}

/// Integer formats whose `minimum`/`maximum` only restate the type's range.
fn natural_bounds(format: &str) -> Option<(Option<f64>, Option<f64>)> {
    Some(match format {
        "uint8" => (Some(0.0), Some(255.0)),
        "uint16" => (Some(0.0), Some(65535.0)),
        "uint32" => (Some(0.0), Some(4294967295.0)),
        "uint64" | "uint" | "uint128" => (Some(0.0), None),
        "int8" => (Some(-128.0), Some(127.0)),
        "int16" => (Some(-32768.0), Some(32767.0)),
        "int32" => (Some(-2147483648.0), Some(2147483647.0)),
        _ => return None,
    })
}

/// Constraints worth showing in a doc comment: `(tag, value)` pairs such
/// as `("minLength", "1")` or `("default", "20")`.
pub(crate) fn facts(schema: &Value) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let Some(obj) = schema.as_object() else {
        return out;
    };
    let format = obj.get("format").and_then(Value::as_str);
    let bounds = format.and_then(natural_bounds);
    if let Some(f) = format.filter(|f| {
        natural_bounds(f).is_none() && !matches!(*f, "int" | "int64" | "double" | "float")
    }) {
        out.push(("format", f.to_string()));
    }
    for (key, which) in [("minimum", 0), ("maximum", 1)] {
        if let Some(v) = obj.get(key).and_then(Value::as_f64) {
            let natural = bounds.and_then(|b| if which == 0 { b.0 } else { b.1 });
            if natural != Some(v) {
                out.push((
                    if which == 0 { "minimum" } else { "maximum" },
                    obj[key].to_string(),
                ));
            }
        }
    }
    for key in [
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "pattern",
    ] {
        if let Some(v) = obj.get(key) {
            let v = match v {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            };
            out.push((static_key(key), v));
        }
    }
    if let Some(d) = obj.get("default").filter(|d| !d.is_null()) {
        out.push(("default", d.to_string()));
    }
    out
}

fn static_key(key: &str) -> &'static str {
    match key {
        "exclusiveMinimum" => "exclusiveMinimum",
        "exclusiveMaximum" => "exclusiveMaximum",
        "minLength" => "minLength",
        "maxLength" => "maxLength",
        "minItems" => "minItems",
        "maxItems" => "maxItems",
        _ => "pattern",
    }
}

/// The doc comment of a property: its description, constraints and
/// deprecation as JSDoc tags.
pub(crate) fn prop_doc(prop: &Value) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if let Some(d) = prop.get("description").and_then(Value::as_str) {
        let d = d.trim();
        if !d.is_empty() {
            lines.push(d.to_string());
        }
    }
    let facts = facts(prop);
    for (tag, value) in facts {
        lines.push(format!("@{tag} {value}"));
    }
    if prop.get("deprecated").and_then(Value::as_bool) == Some(true) {
        lines.push("@deprecated".into());
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The doc comment of a named schema: its description, plus one line per
/// documented enum variant.
pub(crate) fn schema_doc(schema: &Value) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if let Some(d) = schema.get("description").and_then(Value::as_str) {
        let d = d.trim();
        if !d.is_empty() {
            lines.push(d.to_string());
        }
    }
    let variants: Vec<String> = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let c = v.get("const")?;
            let d = v.get("description").and_then(Value::as_str)?;
            Some(format!("- `{c}`: {}", d.trim()))
        })
        .collect();
    if !variants.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(variants);
    }
    if schema.get("deprecated").and_then(Value::as_bool) == Some(true) {
        lines.push("@deprecated".into());
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Writes `text` as a `/** */` comment at `indent`.
pub(crate) fn doc_comment(out: &mut String, indent: &str, text: Option<&str>) {
    let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
        return;
    };
    let text = escape_comment(text);
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

/// Text safe inside a block comment.
pub(crate) fn escape_comment(text: &str) -> String {
    text.replace("*/", "*\\/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(schema: Value) -> String {
        let empty = Map::new();
        Types::new(&empty, true).ts(&schema, "")
    }

    #[test]
    fn schema_types() {
        assert_eq!(ts(json!({"type": ["string", "null"]})), "string | null");
        assert_eq!(
            ts(json!({"type": "string", "nullable": true})),
            "string | null"
        );
        assert_eq!(ts(json!({"type": "string", "format": "binary"})), "Blob");
        assert_eq!(
            ts(json!({"type": ["array", "null"], "items": {"type": "string"}})),
            "string[] | null"
        );
        assert_eq!(
            ts(
                json!({"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "null"}]}})
            ),
            "(string | null)[]"
        );
        // Unknown refs don't produce undeclared names.
        assert_eq!(ts(json!({"$ref": "#/$defs/Nope"})), "unknown");
        assert_eq!(
            ts(json!({"enum": ["a", "b", null]})),
            "\"a\" | \"b\" | null"
        );
        assert_eq!(ts(json!({"allOf": [{"type": "string"}]})), "string");
        assert_eq!(
            ts(json!({"type": "object", "additionalProperties": {"type": "integer"}})),
            "Record<string, number>"
        );
        assert_eq!(
            ts(json!({"type": "object", "additionalProperties": false})),
            "Record<string, never>"
        );
        assert_eq!(
            ts(json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}]})),
            "[string, number]"
        );
        assert_eq!(ts(json!({})), "unknown");
        assert_eq!(
            ts(
                json!({"type": "object", "properties": {"a-b": {"type": "string", "description": "Dashed."}, "n": {"type": "integer", "minimum": 1}}, "required": ["n"]})
            ),
            "{\n  /** Dashed. */\n  \"a-b\"?: string;\n  /** @minimum 1 */\n  n: number;\n}"
        );
        let empty = Map::new();
        assert_eq!(
            Types::new(&empty, false).ts(
                &json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "object", "properties": {"c": {"type": "boolean"}}}}, "required": ["a"]}),
                ""
            ),
            "{ a: string; b?: { c?: boolean } }"
        );
    }

    #[test]
    fn unique_names_and_refs() {
        let schemas = json!({"Page_User": {"type": "string"}, "PageUser": {"type": "number"}, "Error": {"type": "object", "properties": {"error": {"type": "string"}}}});
        let schemas = schemas.as_object().unwrap();
        let t = Types::new(schemas, true);
        assert_eq!(
            t.ts(&json!({"$ref": "#/components/schemas/PageUser"}), ""),
            "PageUser"
        );
        assert_eq!(
            t.ts(&json!({"$ref": "#/components/schemas/Page_User"}), ""),
            "PageUser2"
        );
        assert_eq!(
            t.ts(&json!({"$ref": "#/components/schemas/Error"}), ""),
            "ErrorBody"
        );
        assert_eq!(
            t.resolve(&json!({"$ref": "#/components/schemas/PageUser"})),
            &json!({"type": "number"})
        );
    }

    #[test]
    fn docs() {
        assert_eq!(
            prop_doc(&json!({"type": "integer", "format": "uint8", "minimum": 0, "maximum": 5, "default": 1})).as_deref(),
            Some("@maximum 5\n@default 1")
        );
        assert_eq!(
            prop_doc(&json!({"type": "string", "format": "email", "maxLength": 80, "deprecated": true, "description": "Mail."})).as_deref(),
            Some("Mail.\n@format email\n@maxLength 80\n@deprecated")
        );
        assert_eq!(
            schema_doc(&json!({"description": "State.", "oneOf": [{"const": "a", "description": "First."}, {"const": "b"}]})).as_deref(),
            Some("State.\n\n- `\"a\"`: First.")
        );
    }
}
