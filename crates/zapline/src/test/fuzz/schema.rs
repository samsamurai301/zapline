//! A JSON Schema validator for the subset schemars and the OpenAPI
//! generator emit (draft 2020-12), plus `$ref` resolution and the coercion
//! of wire strings (path, query, header, form values) into typed values.
//!
//! Supported: `$ref`, `type` (single or list), `enum`, `const`, `allOf`,
//! `anyOf`, `oneOf`, `not`, `properties`, `required`,
//! `additionalProperties`, `minProperties`/`maxProperties`, `items`,
//! `prefixItems`, `minItems`/`maxItems`, `uniqueItems`,
//! `minLength`/`maxLength` (in code points), `minimum`/`maximum`,
//! `exclusiveMinimum`/`exclusiveMaximum` (2020-12 numeric form),
//! `multipleOf`, OpenAPI 3.0 `nullable`, and the integer and float range
//! implied by `format` (`int8` ... `uint64`, `float`). String formats and
//! `pattern` are annotations here, as the 2020-12 default says.

use std::borrow::Cow;

use serde_json::{Map, Value};

/// Nesting deeper than this is treated as valid, which also stops
/// recursive `$ref`s.
const MAX_DEPTH: usize = 64;

/// A number as JSON Schema compares it: integers exactly, others as `f64`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Num {
    Int(i128),
    Float(f64),
}

impl Num {
    pub(crate) fn of(n: &serde_json::Number) -> Num {
        if let Some(i) = n.as_i64() {
            Num::Int(i.into())
        } else if let Some(u) = n.as_u64() {
            Num::Int(u.into())
        } else {
            Num::Float(n.as_f64().unwrap_or(f64::NAN))
        }
    }

    pub(crate) fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }

    fn is_integer(self) -> bool {
        match self {
            Num::Int(_) => true,
            Num::Float(f) => f.is_finite() && f.fract() == 0.0,
        }
    }

    /// An integral float as an exact integer, when it fits: `2^64` parsed
    /// from a wire string must compare greater than `u64::MAX`.
    fn exact(self) -> Num {
        match self {
            Num::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 1.0e38 => {
                Num::Int(f as i128)
            }
            n => n,
        }
    }

    /// `self < other`, exactly for integers.
    fn lt(self, other: Num) -> bool {
        match (self.exact(), other.exact()) {
            (Num::Int(a), Num::Int(b)) => a < b,
            (a, b) => a.as_f64() < b.as_f64(),
        }
    }

    fn le(self, other: Num) -> bool {
        match (self.exact(), other.exact()) {
            (Num::Int(a), Num::Int(b)) => a <= b,
            (a, b) => a.as_f64() <= b.as_f64(),
        }
    }
}

/// The inclusive integer range a `format` implies.
pub(crate) fn format_range(format: &str) -> Option<(i128, i128)> {
    Some(match format {
        "int8" => (i8::MIN.into(), i8::MAX.into()),
        "int16" => (i16::MIN.into(), i16::MAX.into()),
        "int32" => (i32::MIN.into(), i32::MAX.into()),
        "int64" | "int" => (i64::MIN.into(), i64::MAX.into()),
        "uint8" => (0, u8::MAX.into()),
        "uint16" => (0, u16::MAX.into()),
        "uint32" => (0, u32::MAX.into()),
        "uint64" | "uint" => (0, u64::MAX.into()),
        "int128" => (i128::MIN, i128::MAX),
        "uint128" => (0, i128::MAX),
        _ => return None,
    })
}

/// Follows `$ref`s into an OpenAPI document.
#[derive(Clone, Copy)]
pub(crate) struct Resolver<'a> {
    doc: &'a Value,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(doc: &'a Value) -> Self {
        Resolver { doc }
    }

    /// The schema a `$ref` points at (`None` for external or dangling refs).
    pub(crate) fn lookup(&self, reference: &str) -> Option<&'a Value> {
        let pointer = reference.strip_prefix('#')?;
        let decoded = percent_encoding::percent_decode_str(pointer).decode_utf8_lossy();
        self.doc.pointer(&decoded)
    }

    /// `schema` with top-level `$ref`s followed (at most a few hops).
    pub(crate) fn resolve<'b>(&self, mut schema: &'b Value) -> &'b Value
    where
        'a: 'b,
    {
        for _ in 0..16 {
            match schema.get("$ref").and_then(Value::as_str) {
                Some(r) => match self.lookup(r) {
                    Some(target) => schema = target,
                    None => return schema,
                },
                None => return schema,
            }
        }
        schema
    }

    /// Validate `value` against `schema`; each error is `pointer: message`.
    pub(crate) fn validate(&self, schema: &Value, value: &Value) -> Vec<String> {
        let mut errors = Vec::new();
        self.check(schema, value, &mut String::new(), 0, &mut errors);
        errors
    }

    pub(crate) fn is_valid(&self, schema: &Value, value: &Value) -> bool {
        let mut errors = Vec::new();
        self.check(schema, value, &mut String::new(), 0, &mut errors);
        errors.is_empty()
    }

    fn check(
        &self,
        schema: &Value,
        value: &Value,
        at: &mut String,
        depth: usize,
        errors: &mut Vec<String>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let s = match schema {
            Value::Bool(true) => return,
            Value::Bool(false) => {
                return push(errors, at, "no value is allowed here".into());
            }
            Value::Object(s) => s,
            _ => return,
        };

        if let Some(r) = s.get("$ref").and_then(Value::as_str) {
            if let Some(target) = self.lookup(r) {
                self.check(target, value, at, depth + 1, errors);
            }
        }

        let nullable = s.get("nullable") == Some(&Value::Bool(true));
        if value.is_null() && nullable {
            return;
        }
        if let Some(t) = s.get("type") {
            let ok = match t {
                Value::String(t) => has_type(value, t),
                Value::Array(ts) => ts
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|t| has_type(value, t)),
                _ => true,
            };
            if !ok {
                push(
                    errors,
                    at,
                    format!("expected {}, got {}", type_list(t), describe(value)),
                );
                // Further keywords would only repeat the mismatch.
                return;
            }
        }
        if let Some(c) = s.get("const") {
            if !json_eq(c, value) {
                push(errors, at, format!("expected {c}, got {}", short(value)));
            }
        }
        if let Some(Value::Array(options)) = s.get("enum") {
            if !options.iter().any(|o| json_eq(o, value)) {
                push(
                    errors,
                    at,
                    format!(
                        "{} is not one of {}",
                        short(value),
                        short(&Value::Array(options.clone()))
                    ),
                );
            }
        }

        if let Some(Value::Array(all)) = s.get("allOf") {
            for sub in all {
                self.check(sub, value, at, depth + 1, errors);
            }
        }
        if let Some(Value::Array(any)) = s.get("anyOf") {
            if !any.iter().any(|sub| self.passes(sub, value, depth)) {
                push(
                    errors,
                    at,
                    format!("{} matches none of the `anyOf` schemas", short(value)),
                );
            }
        }
        if let Some(Value::Array(one)) = s.get("oneOf") {
            let n = one
                .iter()
                .filter(|sub| self.passes(sub, value, depth))
                .count();
            if n != 1 {
                push(
                    errors,
                    at,
                    format!(
                        "{} matches {n} of the `oneOf` schemas (expected exactly 1)",
                        short(value)
                    ),
                );
            }
        }
        if let Some(not) = s.get("not") {
            if self.passes(not, value, depth) {
                push(
                    errors,
                    at,
                    format!("{} matches a `not` schema", short(value)),
                );
            }
        }

        match value {
            Value::Object(obj) => self.check_object(s, obj, at, depth, errors),
            Value::Array(items) => self.check_array(s, items, at, depth, errors),
            Value::String(text) => {
                let len = text.chars().count() as u64;
                if let Some(min) = s.get("minLength").and_then(Value::as_u64) {
                    if len < min {
                        push(
                            errors,
                            at,
                            format!("string of {len} characters is shorter than minLength {min}"),
                        );
                    }
                }
                if let Some(max) = s.get("maxLength").and_then(Value::as_u64) {
                    if len > max {
                        push(
                            errors,
                            at,
                            format!("string of {len} characters is longer than maxLength {max}"),
                        );
                    }
                }
            }
            Value::Number(n) => {
                let n = Num::of(n);
                let bound = |k: &str| s.get(k).and_then(Value::as_number).map(Num::of);
                if let Some(min) = bound("minimum") {
                    if n.lt(min) {
                        push(
                            errors,
                            at,
                            format!("{} is less than the minimum {}", short(value), show(min)),
                        );
                    }
                }
                if let Some(max) = bound("maximum") {
                    if max.lt(n) {
                        push(
                            errors,
                            at,
                            format!("{} is greater than the maximum {}", short(value), show(max)),
                        );
                    }
                }
                if let Some(min) = bound("exclusiveMinimum") {
                    if n.le(min) {
                        push(
                            errors,
                            at,
                            format!("{} is not greater than {}", short(value), show(min)),
                        );
                    }
                }
                if let Some(max) = bound("exclusiveMaximum") {
                    if max.le(n) {
                        push(
                            errors,
                            at,
                            format!("{} is not less than {}", short(value), show(max)),
                        );
                    }
                }
                if let Some(m) = bound("multipleOf") {
                    let q = n.as_f64() / m.as_f64();
                    if m.as_f64() > 0.0 && (q - q.round()).abs() > 1e-9 {
                        push(
                            errors,
                            at,
                            format!("{} is not a multiple of {}", short(value), show(m)),
                        );
                    }
                }
                match s.get("format").and_then(Value::as_str) {
                    Some(f) if is_integer_type(s) => {
                        if let Some((lo, hi)) = format_range(f) {
                            if n.lt(Num::Int(lo)) || Num::Int(hi).lt(n) {
                                push(
                                    errors,
                                    at,
                                    format!("{} is out of range for {f}", short(value)),
                                );
                            }
                        }
                    }
                    Some("float") if !n.as_f64().is_nan() && n.as_f64().abs() > f32::MAX as f64 => {
                        push(
                            errors,
                            at,
                            format!("{} is out of range for float", short(value)),
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn passes(&self, schema: &Value, value: &Value, depth: usize) -> bool {
        let mut errors = Vec::new();
        self.check(schema, value, &mut String::new(), depth + 1, &mut errors);
        errors.is_empty()
    }

    fn check_object(
        &self,
        s: &Map<String, Value>,
        obj: &Map<String, Value>,
        at: &mut String,
        depth: usize,
        errors: &mut Vec<String>,
    ) {
        if let Some(Value::Array(required)) = s.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !obj.contains_key(name) {
                    push(errors, at, format!("missing required property `{name}`"));
                }
            }
        }
        let n = obj.len() as u64;
        if let Some(min) = s.get("minProperties").and_then(Value::as_u64) {
            if n < min {
                push(
                    errors,
                    at,
                    format!("{n} properties, fewer than minProperties {min}"),
                );
            }
        }
        if let Some(max) = s.get("maxProperties").and_then(Value::as_u64) {
            if n > max {
                push(
                    errors,
                    at,
                    format!("{n} properties, more than maxProperties {max}"),
                );
            }
        }
        let props = s.get("properties").and_then(Value::as_object);
        // Without a regex engine, keys that could match `patternProperties`
        // are not checked against `additionalProperties`.
        let patterns = s.contains_key("patternProperties");
        let additional = s.get("additionalProperties");
        for (k, v) in obj {
            let len = at.len();
            at.push('/');
            at.push_str(&k.replace('~', "~0").replace('/', "~1"));
            match props.and_then(|p| p.get(k)) {
                Some(sub) => self.check(sub, v, at, depth + 1, errors),
                None if patterns => {}
                None => match additional {
                    Some(Value::Bool(false)) => {
                        at.truncate(len);
                        push(errors, at, format!("unexpected property `{k}`"));
                        continue;
                    }
                    Some(sub) => self.check(sub, v, at, depth + 1, errors),
                    None => {}
                },
            }
            at.truncate(len);
        }
    }

    fn check_array(
        &self,
        s: &Map<String, Value>,
        items: &[Value],
        at: &mut String,
        depth: usize,
        errors: &mut Vec<String>,
    ) {
        let n = items.len() as u64;
        if let Some(min) = s.get("minItems").and_then(Value::as_u64) {
            if n < min {
                push(errors, at, format!("{n} items, fewer than minItems {min}"));
            }
        }
        if let Some(max) = s.get("maxItems").and_then(Value::as_u64) {
            if n > max {
                push(errors, at, format!("{n} items, more than maxItems {max}"));
            }
        }
        if s.get("uniqueItems") == Some(&Value::Bool(true)) {
            'outer: for (i, a) in items.iter().enumerate() {
                for b in &items[i + 1..] {
                    if json_eq(a, b) {
                        push(errors, at, format!("duplicate item {}", short(a)));
                        break 'outer;
                    }
                }
            }
        }
        let prefix = s.get("prefixItems").and_then(Value::as_array);
        let rest = s.get("items");
        for (i, item) in items.iter().enumerate() {
            let sub = match prefix.and_then(|p| p.get(i)) {
                Some(sub) => Some(sub),
                None => rest,
            };
            if let Some(sub) = sub {
                let len = at.len();
                at.push('/');
                at.push_str(&i.to_string());
                self.check(sub, item, at, depth + 1, errors);
                at.truncate(len);
            }
        }
    }

    /// The schema of a parameter or form field as the deserializer sees a
    /// wire string: candidates in the order serde would try them.
    pub(crate) fn coerce(&self, schema: &Value, raw: &str) -> Value {
        let types = self.types(schema);
        let try_number = types.is_empty() || types.iter().any(|t| t == "integer" || t == "number");
        let try_bool = types.is_empty() || types.iter().any(|t| t == "boolean");
        let wants_string = types.iter().any(|t| t == "string");
        let mut candidates: Vec<Value> = Vec::new();
        if try_number {
            if let Some(n) = parse_number(raw) {
                candidates.push(n);
            }
        }
        if try_bool && (raw == "true" || raw == "false") {
            candidates.push(Value::Bool(raw == "true"));
        }
        candidates.push(Value::String(raw.to_owned()));
        if wants_string {
            return Value::String(raw.to_owned());
        }
        candidates
            .iter()
            .find(|c| self.is_valid(schema, c))
            .cloned()
            .unwrap_or_else(|| candidates.swap_remove(0))
    }

    /// The JSON types a schema admits (through `$ref`, `anyOf`, `oneOf`,
    /// `allOf`); empty when anything goes.
    pub(crate) fn types(&self, schema: &Value) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_types(schema, &mut out, 0);
        out
    }

    fn collect_types(&self, schema: &Value, out: &mut Vec<String>, depth: usize) {
        if depth > 8 {
            return;
        }
        let schema = self.resolve(schema);
        match schema.get("type") {
            Some(Value::String(t)) => out.push(t.clone()),
            Some(Value::Array(ts)) => {
                out.extend(ts.iter().filter_map(Value::as_str).map(str::to_owned))
            }
            _ => {
                for key in ["anyOf", "oneOf", "allOf"] {
                    if let Some(Value::Array(subs)) = schema.get(key) {
                        for sub in subs {
                            self.collect_types(sub, out, depth + 1);
                        }
                    }
                }
                if let Some(Value::Array(options)) = schema.get("enum") {
                    out.extend(options.iter().map(|v| json_type(v).to_owned()));
                }
                if let Some(c) = schema.get("const") {
                    out.push(json_type(c).to_owned());
                }
            }
        }
    }
}

fn push(errors: &mut Vec<String>, at: &str, msg: String) {
    let at = if at.is_empty() { "body" } else { at };
    errors.push(format!("{at}: {msg}"));
}

/// Parse a wire string the way serde's number parsing would accept it.
pub(crate) fn parse_number(raw: &str) -> Option<Value> {
    if raw.is_empty() || raw.starts_with('+') {
        return None;
    }
    if let Ok(i) = raw.parse::<i64>() {
        return Some(i.into());
    }
    if let Ok(u) = raw.parse::<u64>() {
        return Some(u.into());
    }
    if raw.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        // An integer beyond 64 bits: keep it out of range rather than
        // rounding it into range.
        return raw
            .parse::<f64>()
            .ok()
            .map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number));
    }
    let f: f64 = raw.parse().ok()?;
    serde_json::Number::from_f64(f).map(Value::Number)
}

fn has_type(value: &Value, t: &str) -> bool {
    match t {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(|n| Num::of(n).is_integer()),
        _ => true,
    }
}

fn is_integer_type(s: &Map<String, Value>) -> bool {
    match s.get("type") {
        Some(Value::String(t)) => t == "integer",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "integer"),
        _ => false,
    }
}

pub(crate) fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if Num::of(n).is_integer() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Equality where `1` and `1.0` are the same number.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (Num::of(x), Num::of(y));
            x.le(y) && y.le(x)
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

fn type_list(t: &Value) -> Cow<'_, str> {
    match t {
        Value::String(s) => Cow::Borrowed(s),
        Value::Array(ts) => Cow::Owned(
            ts.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" or "),
        ),
        _ => Cow::Borrowed("?"),
    }
}

fn describe(v: &Value) -> String {
    format!("{} {}", json_type(v), short(v))
}

fn show(n: Num) -> String {
    match n {
        Num::Int(i) => i.to_string(),
        Num::Float(f) => f.to_string(),
    }
}

/// A value for messages, cut to a readable length.
pub(crate) fn short(v: &Value) -> String {
    const MAX: usize = 60;
    let text = v.to_string();
    if text.chars().count() <= MAX {
        return text;
    }
    let cut: String = text.chars().take(MAX).collect();
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errors(schema: Value, value: Value) -> Vec<String> {
        let doc = json!({ "components": { "schemas": { "Pet": {
            "type": "object",
            "properties": { "name": { "type": "string", "minLength": 1 }, "age": { "type": "integer", "format": "uint8", "minimum": 0 } },
            "required": ["name"],
            "additionalProperties": false
        } } } });
        Resolver::new(&doc).validate(&schema, &value)
    }

    #[test]
    fn types_and_ranges() {
        assert!(errors(json!({ "type": "integer" }), json!(3)).is_empty());
        assert!(errors(json!({ "type": "integer" }), json!(3.0)).is_empty());
        assert_eq!(errors(json!({ "type": "integer" }), json!(3.5)).len(), 1);
        assert!(errors(json!({ "type": ["string", "null"] }), json!(null)).is_empty());
        assert_eq!(
            errors(json!({ "type": "integer", "format": "uint8" }), json!(256)).len(),
            1
        );
        assert_eq!(
            errors(json!({ "type": "integer", "format": "uint64" }), json!(-1)).len(),
            1
        );
        assert!(errors(
            json!({ "type": "integer", "format": "uint64" }),
            json!(u64::MAX)
        )
        .is_empty());
        assert_eq!(
            errors(
                json!({ "type": "integer", "format": "uint64" }),
                json!(1.9e19)
            )
            .len(),
            1
        );
        assert_eq!(
            errors(json!({ "type": "number", "exclusiveMinimum": 0 }), json!(0)).len(),
            1
        );
        assert_eq!(
            errors(json!({ "type": "string", "maxLength": 2 }), json!("日本語")).len(),
            1
        );
        assert!(errors(json!({ "type": "string", "maxLength": 3 }), json!("日本語")).is_empty());
        assert_eq!(
            errors(json!({ "type": "number", "format": "float" }), json!(1e300)).len(),
            1
        );
    }

    #[test]
    fn objects_refs_and_combinators() {
        let pet = json!({ "$ref": "#/components/schemas/Pet" });
        assert!(errors(pet.clone(), json!({ "name": "rex", "age": 3 })).is_empty());
        let e = errors(pet.clone(), json!({ "age": 300, "x": 1 }));
        assert_eq!(e.len(), 3, "{e:?}");
        assert!(e.iter().any(|e| e.starts_with("/age:")), "{e:?}");
        assert!(e
            .iter()
            .any(|e| e.contains("missing required property `name`")));
        assert!(e.iter().any(|e| e.contains("unexpected property `x`")));

        let one = json!({ "oneOf": [{ "const": "a" }, { "type": "string" }] });
        assert_eq!(errors(one.clone(), json!("a")).len(), 1);
        assert!(errors(one, json!("b")).is_empty());
        let any = json!({ "anyOf": [{ "type": "integer" }, { "type": "null" }] });
        assert!(errors(any.clone(), json!(null)).is_empty());
        assert_eq!(errors(any, json!("x")).len(), 1);
        let items = json!({ "type": "array", "items": { "type": "integer" }, "maxItems": 2, "uniqueItems": true });
        assert_eq!(
            errors(items.clone(), json!([1, "x"])),
            vec!["/1: expected integer, got string \"x\""]
        );
        assert_eq!(errors(items, json!([1, 1, 1])).len(), 2);
    }

    #[test]
    fn wire_strings_are_coerced() {
        let doc = json!({});
        let r = Resolver::new(&doc);
        assert_eq!(r.coerce(&json!({ "type": "integer" }), "42"), json!(42));
        assert_eq!(r.coerce(&json!({ "type": "integer" }), "+42"), json!("+42"));
        assert_eq!(r.coerce(&json!({ "type": "string" }), "42"), json!("42"));
        assert_eq!(r.coerce(&json!({ "type": "boolean" }), "true"), json!(true));
        let big = r.coerce(
            &json!({ "type": "integer", "format": "uint64" }),
            "18446744073709551616",
        );
        assert!(!r.is_valid(&json!({ "type": "integer", "format": "uint64" }), &big));
    }
}
