//! Values from JSON Schemas: typical valid values, boundary values that are
//! still valid, and values that break the schema on purpose.
//!
//! Whether a request is valid is decided afterwards by the validator, not
//! by how its values were made; generation only aims.

use serde_json::{json, Map, Value};

use super::schema::{format_range, Num, Resolver};

/// Deeper than this, objects keep only required properties and arrays
/// their minimum length, so recursive schemas terminate.
const MAX_DEPTH: usize = 6;

/// Length of a "long" string when the schema sets no `maxLength`.
const LONG_STRING: usize = 1024;

/// How values are made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// Realistic values: examples when the schema has some, short strings,
    /// small numbers, about half of the optional properties.
    Typical,
    /// Every optional property and at least one array item, so that every
    /// part of the schema has a value to mutate.
    Full,
    /// Like `Typical`, but roughly a third of the scalars are boundary
    /// values.
    Random,
}

/// One step of a location inside a JSON value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    Key(String),
    Index(usize),
}

/// A location in a generated value, with the schema that governs it.
pub(crate) struct Target {
    pub(crate) at: Vec<Seg>,
    pub(crate) schema: Value,
    /// `Some(required)` for object properties (they can be removed).
    pub(crate) property: Option<bool>,
}

impl Target {
    /// `/address/zip` style, for case labels.
    pub(crate) fn pointer(&self) -> String {
        pointer(&self.at)
    }
}

pub(crate) fn pointer(at: &[Seg]) -> String {
    let mut out = String::new();
    for seg in at {
        out.push('/');
        match seg {
            Seg::Key(k) => out.push_str(k),
            Seg::Index(i) => out.push_str(&i.to_string()),
        }
    }
    out
}

pub(crate) struct Gen<'a> {
    pub(crate) r: Resolver<'a>,
    pub(crate) rng: fastrand::Rng,
}

impl<'a> Gen<'a> {
    pub(crate) fn new(r: Resolver<'a>, seed: u64) -> Self {
        Gen {
            r,
            rng: fastrand::Rng::with_seed(seed),
        }
    }

    /// A value for `schema`.
    pub(crate) fn value(&mut self, schema: &Value, mode: Mode) -> Value {
        self.gen(schema, mode, 0)
    }

    fn gen(&mut self, schema: &Value, mode: Mode, depth: usize) -> Value {
        let schema = self.r.resolve(schema);
        let s = match schema {
            Value::Object(s) => s,
            Value::Bool(false) => return Value::Null,
            _ => return self.any_scalar(),
        };
        if let Some(c) = s.get("const") {
            return c.clone();
        }
        if let Some(Value::Array(options)) = s.get("enum") {
            if !options.is_empty() {
                return options[self.rng.usize(..options.len())].clone();
            }
        }
        if mode == Mode::Typical || mode == Mode::Full {
            let examples = examples(s);
            if !examples.is_empty() && self.rng.u8(..3) != 0 {
                return examples[self.rng.usize(..examples.len())].clone();
            }
        }
        if s.contains_key("allOf") {
            let merged = self.merge_all_of(s);
            return self.gen(&merged, mode, depth + 1);
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(Value::Array(branches)) = s.get(key) {
                if branches.is_empty() {
                    continue;
                }
                let picked = self.pick_branch(branches, mode);
                return self.gen(picked, mode, depth + 1);
            }
        }
        match self.pick_type(s, mode).as_str() {
            "null" => Value::Null,
            "boolean" => Value::Bool(self.rng.bool()),
            "integer" => {
                if mode == Mode::Random && self.rng.u8(..3) == 0 {
                    self.pick(int_edges(s))
                } else {
                    int_value(self.typical_int(s))
                }
            }
            "number" => {
                if mode == Mode::Random && self.rng.u8(..3) == 0 {
                    self.pick(number_edges(s))
                } else {
                    self.typical_number(s)
                }
            }
            "string" => {
                if mode == Mode::Random && self.rng.u8(..3) == 0 {
                    self.pick(string_edges(s))
                } else {
                    Value::String(self.typical_string(s))
                }
            }
            "array" => self.array(s, mode, depth),
            "object" => self.object(s, mode, depth),
            _ => self.any_scalar(),
        }
    }

    fn pick(&mut self, mut options: Vec<Value>) -> Value {
        if options.is_empty() {
            return Value::Null;
        }
        let i = self.rng.usize(..options.len());
        options.swap_remove(i)
    }

    fn any_scalar(&mut self) -> Value {
        match self.rng.u8(..3) {
            0 => Value::String(self.word(1, 8)),
            1 => json!(self.rng.i64(-100..=100)),
            _ => Value::Bool(self.rng.bool()),
        }
    }

    /// Prefer non-null branches: `null` alternatives get a turn in `Random`.
    fn pick_branch<'v>(&mut self, branches: &'v [Value], mode: Mode) -> &'v Value {
        let non_null: Vec<&Value> = branches
            .iter()
            .filter(|b| self.r.resolve(b).get("type") != Some(&json!("null")))
            .collect();
        if non_null.is_empty() || (mode == Mode::Random && self.rng.u8(..4) == 0) {
            return &branches[self.rng.usize(..branches.len())];
        }
        non_null[self.rng.usize(..non_null.len())]
    }

    fn pick_type(&mut self, s: &Map<String, Value>, mode: Mode) -> String {
        let types: Vec<&str> = match s.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if types.is_empty() {
            let inferred = if s.contains_key("properties") || s.contains_key("additionalProperties")
            {
                "object"
            } else if s.contains_key("items") || s.contains_key("prefixItems") {
                "array"
            } else if s.contains_key("minLength") || s.contains_key("maxLength") {
                "string"
            } else if s.contains_key("minimum") || s.contains_key("maximum") {
                "number"
            } else {
                ""
            };
            return inferred.to_owned();
        }
        let non_null: Vec<&str> = types.iter().copied().filter(|t| *t != "null").collect();
        let null_turn = mode == Mode::Random && self.rng.u8(..4) == 0;
        if non_null.is_empty() || (null_turn && non_null.len() < types.len()) {
            return "null".to_owned();
        }
        non_null[self.rng.usize(..non_null.len())].to_owned()
    }

    /// A lowercase alphanumeric word of `min..=max` characters.
    fn word(&mut self, min: usize, max: usize) -> String {
        let n = if max > min {
            self.rng.usize(min..=max)
        } else {
            min
        };
        (0..n)
            .map(|_| {
                let c = self.rng.alphanumeric();
                c.to_ascii_lowercase()
            })
            .collect()
    }

    fn typical_string(&mut self, s: &Map<String, Value>) -> String {
        let (min, max) = lengths(s);
        if let Some(f) = s.get("format").and_then(Value::as_str) {
            if let Some(v) = self.formatted(f) {
                let n = v.chars().count();
                if n >= min && max.is_none_or(|m| n <= m) {
                    return v;
                }
            }
        }
        let hi = max.unwrap_or(usize::MAX).min(min.max(1) + 11);
        self.word(min, hi.max(min))
    }

    /// A valid example of a string `format`.
    fn formatted(&mut self, format: &str) -> Option<String> {
        let n = self.rng.u32(1..10_000);
        Some(match format {
            "email" | "idn-email" => format!("user{n}@example.com"),
            "uri" | "url" | "iri" => format!("https://example.com/items/{n}"),
            "uri-reference" | "iri-reference" => format!("/items/{n}"),
            "uuid" => {
                let a = self.rng.u128(..);
                let h = format!("{a:032x}");
                format!(
                    "{}-{}-4{}-a{}-{}",
                    &h[..8],
                    &h[8..12],
                    &h[13..16],
                    &h[17..20],
                    &h[20..32]
                )
            }
            "date-time" => format!("2024-0{}-1{}T12:34:56Z", 1 + n % 9, n % 10),
            "date" => format!("2024-0{}-1{}", 1 + n % 9, n % 10),
            "time" => "12:34:56Z".to_owned(),
            "ipv4" => format!("192.0.2.{}", n % 255),
            "ipv6" => format!("2001:db8::{:x}", n),
            "hostname" | "idn-hostname" => format!("host{n}.example.com"),
            "byte" => "aGVsbG8=".to_owned(),
            "duration" => "PT1H".to_owned(),
            _ => return None,
        })
    }

    fn typical_int(&mut self, s: &Map<String, Value>) -> i128 {
        let (lo, hi) = int_bounds(s);
        let (lo, hi) = (lo.max(-1000).min(hi), hi.min(lo.max(0) + 1000).max(lo));
        let v = if lo >= hi {
            lo
        } else {
            lo + i128::from(self.rng.u64(..=((hi - lo) as u64).min(u64::MAX - 1)))
        };
        round_to_multiple(s, v).unwrap_or(v)
    }

    fn typical_number(&mut self, s: &Map<String, Value>) -> Value {
        let (lo, hi) = number_bounds(s);
        let (lo, hi) = (lo.max(-1000.0), hi.min(1000.0));
        if lo > hi {
            return float_value(lo);
        }
        let v = lo + (hi - lo) * self.rng.f64();
        let v = (v * 100.0).round() / 100.0;
        float_value(v.clamp(lo, hi))
    }

    fn array(&mut self, s: &Map<String, Value>, mode: Mode, depth: usize) -> Value {
        let min = s.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max = s
            .get("maxItems")
            .and_then(Value::as_u64)
            .map(|m| m as usize);
        let cap = |n: usize| max.map_or(n, |m| n.min(m));
        let n = if depth >= MAX_DEPTH {
            min
        } else {
            match mode {
                Mode::Full => cap(min.max(1)),
                Mode::Typical => {
                    let hi = cap(min + 3);
                    self.rng.usize(min..=hi.max(min))
                }
                Mode::Random => {
                    let hi = cap(min + 12);
                    self.rng.usize(min..=hi.max(min))
                }
            }
        };
        let prefix = s.get("prefixItems").and_then(Value::as_array);
        let items = s.get("items");
        let unique = s.get("uniqueItems") == Some(&Value::Bool(true));
        let mut out: Vec<Value> = Vec::with_capacity(n);
        for i in 0..n {
            let sub = match prefix.and_then(|p| p.get(i)) {
                Some(sub) => sub,
                None => match items {
                    Some(sub) => sub,
                    None => {
                        out.push(self.any_scalar());
                        continue;
                    }
                },
            };
            let mut v = self.gen(sub, mode, depth + 1);
            let mut tries = 0;
            while unique && out.contains(&v) && tries < 8 {
                v = self.gen(sub, Mode::Random, depth + 1);
                tries += 1;
            }
            out.push(v);
        }
        Value::Array(out)
    }

    fn object(&mut self, s: &Map<String, Value>, mode: Mode, depth: usize) -> Value {
        let required = required(s);
        let mut out = Map::new();
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            for (k, sub) in props {
                let include = required.contains(&k.as_str())
                    || (depth < MAX_DEPTH
                        && match mode {
                            Mode::Full => true,
                            Mode::Typical | Mode::Random => self.rng.bool(),
                        });
                if include {
                    let v = self.gen(sub, mode, depth + 1);
                    out.insert(k.clone(), v);
                }
            }
        }
        // Required names the schema does not describe still need a value.
        for k in &required {
            if !out.contains_key(*k) {
                out.insert((*k).to_owned(), self.any_scalar());
            }
        }
        if let Some(extra) = s.get("additionalProperties").filter(|a| a.is_object()) {
            let n = match mode {
                _ if depth >= MAX_DEPTH => 0,
                Mode::Full => 1,
                Mode::Typical => self.rng.usize(0..=2),
                Mode::Random => self.rng.usize(0..=4),
            };
            for _ in 0..n {
                let key = self.word(3, 8);
                let v = self.gen(extra, mode, depth + 1);
                out.insert(key, v);
            }
        }
        let min = s.get("minProperties").and_then(Value::as_u64).unwrap_or(0) as usize;
        while out.len() < min {
            let key = self.word(4, 8);
            out.insert(key, self.any_scalar());
        }
        Value::Object(out)
    }

    /// One schema from an `allOf` (and the keywords around it).
    pub(crate) fn merge_all_of(&self, s: &Map<String, Value>) -> Value {
        let mut merged = Map::new();
        let mut props = Map::new();
        let mut req: Vec<Value> = Vec::new();
        self.merge_into(s, &mut merged, &mut props, &mut req, 0);
        if !props.is_empty() {
            merged.insert("properties".into(), Value::Object(props));
        }
        if !req.is_empty() {
            merged.insert("required".into(), Value::Array(req));
        }
        Value::Object(merged)
    }

    fn merge_into(
        &self,
        s: &Map<String, Value>,
        merged: &mut Map<String, Value>,
        props: &mut Map<String, Value>,
        req: &mut Vec<Value>,
        depth: usize,
    ) {
        if depth > 8 {
            return;
        }
        for (k, v) in s {
            match k.as_str() {
                "allOf" => {
                    for sub in v.as_array().into_iter().flatten() {
                        if let Value::Object(sub) = self.r.resolve(sub) {
                            self.merge_into(sub, merged, props, req, depth + 1);
                        }
                    }
                }
                "properties" => {
                    for (name, p) in v.as_object().into_iter().flatten() {
                        props.insert(name.clone(), p.clone());
                    }
                }
                "required" => {
                    for name in v.as_array().into_iter().flatten() {
                        if !req.contains(name) {
                            req.push(name.clone());
                        }
                    }
                }
                "$ref" => {
                    if let Some(Value::Object(sub)) = self.r.lookup(v.as_str().unwrap_or_default())
                    {
                        self.merge_into(sub, merged, props, req, depth + 1);
                    }
                }
                _ => {
                    merged.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }

    /// Every location of `value` (generated for `schema`) worth mutating.
    pub(crate) fn targets(&self, schema: &Value, value: &Value) -> Vec<Target> {
        let mut out = Vec::new();
        self.walk(schema, value, &mut Vec::new(), None, &mut out, 0);
        out
    }

    fn walk(
        &self,
        schema: &Value,
        value: &Value,
        at: &mut Vec<Seg>,
        property: Option<bool>,
        out: &mut Vec<Target>,
        depth: usize,
    ) {
        let schema = self.r.resolve(schema);
        out.push(Target {
            at: at.clone(),
            schema: schema.clone(),
            property,
        });
        if depth >= 4 {
            return;
        }
        let Some(s) = schema.as_object() else {
            return;
        };
        // Descend through the branch the value was made from.
        let merged;
        let mut eff = s;
        if s.contains_key("allOf") {
            merged = self.merge_all_of(s);
            if let Value::Object(m) = &merged {
                eff = m;
            }
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(Value::Array(branches)) = eff.get(key) {
                if let Some(b) = branches.iter().find(|b| self.r.is_valid(b, value)) {
                    if let Value::Object(b) = self.r.resolve(b) {
                        return self.walk_children(b, value, at, out, depth);
                    }
                }
                return;
            }
        }
        self.walk_children(eff, value, at, out, depth)
    }

    fn walk_children(
        &self,
        s: &Map<String, Value>,
        value: &Value,
        at: &mut Vec<Seg>,
        out: &mut Vec<Target>,
        depth: usize,
    ) {
        match value {
            Value::Object(obj) => {
                let props = s.get("properties").and_then(Value::as_object);
                let required = required(s);
                let extra = s.get("additionalProperties").filter(|a| a.is_object());
                for (k, v) in obj {
                    let Some(sub) = props.and_then(|p| p.get(k)).or(extra) else {
                        continue;
                    };
                    at.push(Seg::Key(k.clone()));
                    let req = required.contains(&k.as_str());
                    self.walk(sub, v, at, Some(req), out, depth + 1);
                    at.pop();
                }
            }
            Value::Array(items) => {
                let prefix = s.get("prefixItems").and_then(Value::as_array);
                for (i, v) in items.iter().enumerate().take(2) {
                    let Some(sub) = prefix.and_then(|p| p.get(i)).or_else(|| s.get("items")) else {
                        continue;
                    };
                    at.push(Seg::Index(i));
                    self.walk(sub, v, at, None, out, depth + 1);
                    at.pop();
                }
            }
            _ => {}
        }
    }

    /// Valid values at the edges of `schema`: empty and maximal strings,
    /// unicode, numeric bounds, empty and large arrays, minimal and full
    /// objects.
    pub(crate) fn edges(&mut self, schema: &Value) -> Vec<(Value, &'static str)> {
        let schema = self.r.resolve(schema);
        let Some(s) = schema.as_object() else {
            return Vec::new();
        };
        if s.contains_key("const") {
            return Vec::new();
        }
        if let Some(Value::Array(options)) = s.get("enum") {
            return options
                .iter()
                .take(16)
                .map(|o| (o.clone(), "enum value"))
                .collect();
        }
        let mut out: Vec<(Value, &'static str)> = Vec::new();
        for key in ["oneOf", "anyOf"] {
            if let Some(Value::Array(branches)) = s.get(key) {
                for b in branches.iter().take(8) {
                    out.extend(self.edges(b));
                }
                return out;
            }
        }
        if s.contains_key("allOf") {
            let merged = self.merge_all_of(s);
            return self.edges(&merged);
        }
        let types: Vec<String> = match s.get("type") {
            Some(Value::String(t)) => vec![t.clone()],
            Some(Value::Array(ts)) => ts
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => vec![self.pick_type(s, Mode::Typical)],
        };
        for t in types {
            match t.as_str() {
                "null" => out.push((Value::Null, "null")),
                "boolean" => out.extend([(json!(true), "boolean"), (json!(false), "boolean")]),
                "integer" => out.extend(int_edges(s).into_iter().map(|v| (v, "boundary integer"))),
                "number" => out.extend(number_edges(s).into_iter().map(|v| (v, "boundary number"))),
                "string" => out.extend(string_edges(s).into_iter().map(|v| (v, "boundary string"))),
                "array" => {
                    let min = s.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let max = s
                        .get("maxItems")
                        .and_then(Value::as_u64)
                        .map(|m| m as usize);
                    let item = s.get("items").cloned().unwrap_or(Value::Bool(true));
                    for (n, label) in [
                        (min, "shortest array"),
                        (max.unwrap_or(64).min(64), "long array"),
                    ] {
                        let items: Vec<Value> =
                            (0..n).map(|_| self.gen(&item, Mode::Typical, 1)).collect();
                        out.push((Value::Array(items), label));
                    }
                }
                "object" => {
                    let mut only_required = Map::new();
                    let full = self.object(s, Mode::Full, 1);
                    let req = required(s);
                    if let Value::Object(f) = &full {
                        for (k, v) in f {
                            if req.contains(&k.as_str()) {
                                only_required.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    out.push((Value::Object(only_required), "only required properties"));
                    out.push((full, "every property"));
                }
                _ => {}
            }
        }
        out
    }

    /// Values that break `schema`: wrong types, numbers out of range,
    /// strings and arrays of the wrong length, unknown enum variants and
    /// properties. Only values the validator rejects are returned.
    pub(crate) fn invalids(&mut self, schema: &Value) -> Vec<(Value, String)> {
        let resolved = self.r.resolve(schema);
        let s = resolved.as_object().cloned().unwrap_or_default();
        let mut out: Vec<(Value, String)> = Vec::new();
        for (v, what) in [
            (json!("not-a-number"), "a string"),
            (json!(12345), "an integer"),
            (json!(4.75), "a fractional number"),
            (json!(true), "a boolean"),
            (Value::Null, "null"),
            (json!([1, "two"]), "an array"),
            (json!({ "unexpected": 1 }), "an object"),
        ] {
            out.push((v, format!("wrong type: {what}")));
        }
        let types = self.r.types(resolved);
        let is = |t: &str| types.iter().any(|x| x == t);
        if is("integer") {
            let (lo, hi) = int_bounds(&s);
            if lo > i128::from(i64::MIN) {
                out.push((int_value(lo - 1), "below the minimum".into()));
            }
            if hi < i128::from(u64::MAX) {
                out.push((int_value(hi + 1), "above the maximum".into()));
            }
            out.push((json!(1.8446744073709552e19), "beyond 64 bits".into()));
            out.push((json!(-1.0e19), "below -2^63".into()));
            out.push((json!(1.5), "not an integer".into()));
        }
        if is("number") {
            let (lo, hi) = number_bounds(&s);
            if lo.is_finite() && lo > f64::MIN {
                out.push((float_value(lo - 1.0), "below the minimum".into()));
            }
            if hi.is_finite() && hi < f64::MAX {
                out.push((float_value(hi + 1.0), "above the maximum".into()));
            }
            out.push((json!(1.0e39), "beyond f32".into()));
        }
        if is("string") {
            let (min, max) = lengths(&s);
            if min > 0 {
                out.push((
                    Value::String("x".repeat(min - 1)),
                    format!("shorter than minLength {min}"),
                ));
            }
            if let Some(max) = max {
                out.push((
                    Value::String("x".repeat(max + 1)),
                    format!("longer than maxLength {max}"),
                ));
            }
            if s.contains_key("enum") || s.contains_key("const") {
                out.push((json!("__not_a_variant__"), "unknown enum value".into()));
            }
        }
        if s.contains_key("enum") || s.contains_key("const") || s.contains_key("oneOf") {
            out.push((json!("__not_a_variant__"), "unknown variant".into()));
        }
        if is("array") {
            let min = s.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            if min > 0 {
                out.push((json!([]), format!("fewer than minItems {min}")));
            }
            if let Some(max) = s.get("maxItems").and_then(Value::as_u64) {
                let item = s.get("items").cloned().unwrap_or(Value::Bool(true));
                let items: Vec<Value> = (0..=max.min(10_000))
                    .map(|_| self.gen(&item, Mode::Typical, 1))
                    .collect();
                out.push((Value::Array(items), format!("more than maxItems {max}")));
            }
        }
        if is("object") {
            if let Value::Object(mut full) = self.object(&s, Mode::Full, 1) {
                full.insert("__unexpected__".into(), json!(1));
                out.push((Value::Object(full), "unknown property".into()));
            }
        }
        let mut seen: Vec<Value> = Vec::new();
        out.retain(|(v, _)| {
            let keep = !seen.contains(v) && !self.r.is_valid(schema, v);
            seen.push(v.clone());
            keep
        });
        out
    }
}

pub(crate) fn required(s: &Map<String, Value>) -> Vec<&str> {
    s.get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn examples(s: &Map<String, Value>) -> Vec<Value> {
    let mut out: Vec<Value> = s
        .get("examples")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for key in ["example", "default"] {
        if let Some(v) = s.get(key) {
            out.push(v.clone());
        }
    }
    out
}

fn lengths(s: &Map<String, Value>) -> (usize, Option<usize>) {
    let min = s.get("minLength").and_then(Value::as_u64).unwrap_or(0) as usize;
    let max = s
        .get("maxLength")
        .and_then(Value::as_u64)
        .map(|m| m as usize);
    (min, max)
}

/// Inclusive integer bounds from `minimum`, `maximum`, their exclusive
/// forms and `format`; unconstrained integers stay within `i64`/`u64`.
pub(crate) fn int_bounds(s: &Map<String, Value>) -> (i128, i128) {
    let (mut lo, mut hi) = s
        .get("format")
        .and_then(Value::as_str)
        .and_then(format_range)
        .unwrap_or((i64::MIN.into(), i64::MAX.into()));
    // JSON numbers beyond 64 bits do not round-trip through serde_json.
    lo = lo.max(i64::MIN.into());
    hi = hi.min(u64::MAX.into());
    let num = |k: &str| s.get(k).and_then(Value::as_number).map(Num::of);
    let ceil = |n: Num| match n {
        Num::Int(i) => i,
        Num::Float(f) => f.ceil() as i128,
    };
    let floor = |n: Num| match n {
        Num::Int(i) => i,
        Num::Float(f) => f.floor() as i128,
    };
    if let Some(n) = num("minimum") {
        lo = lo.max(ceil(n));
    }
    if let Some(n) = num("exclusiveMinimum") {
        lo = lo.max(floor(n) + 1);
    }
    if let Some(n) = num("maximum") {
        hi = hi.min(floor(n));
    }
    if let Some(n) = num("exclusiveMaximum") {
        hi = hi.min(ceil(n) - 1);
    }
    (lo, hi)
}

fn number_bounds(s: &Map<String, Value>) -> (f64, f64) {
    let limit = if s.get("format").and_then(Value::as_str) == Some("float") {
        f32::MAX as f64
    } else {
        f64::MAX
    };
    let (mut lo, mut hi) = (-limit, limit);
    let num = |k: &str| s.get(k).and_then(Value::as_f64);
    if let Some(n) = num("minimum") {
        lo = lo.max(n);
    }
    if let Some(n) = num("exclusiveMinimum") {
        lo = lo.max(n + n.abs().max(1.0) * 1e-9);
    }
    if let Some(n) = num("maximum") {
        hi = hi.min(n);
    }
    if let Some(n) = num("exclusiveMaximum") {
        hi = hi.min(n - n.abs().max(1.0) * 1e-9);
    }
    (lo, hi)
}

fn round_to_multiple(s: &Map<String, Value>, v: i128) -> Option<i128> {
    let m = s.get("multipleOf").and_then(Value::as_u64)?;
    let m = i128::from(m.max(1));
    Some(v - v.rem_euclid(m))
}

pub(crate) fn int_value(v: i128) -> Value {
    if let Ok(i) = i64::try_from(v) {
        json!(i)
    } else if let Ok(u) = u64::try_from(v) {
        json!(u)
    } else {
        float_value(v as f64)
    }
}

fn float_value(v: f64) -> Value {
    serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
}

fn int_edges(s: &Map<String, Value>) -> Vec<Value> {
    let (lo, hi) = int_bounds(s);
    let mut out: Vec<i128> = Vec::new();
    for v in [
        lo,
        lo.saturating_add(1),
        -1,
        0,
        1,
        hi.saturating_sub(1),
        hi,
        i128::from(i32::MAX),
        i128::from(i32::MAX) + 1,
        (1 << 53) + 1,
    ] {
        let v = round_to_multiple(s, v).unwrap_or(v);
        if (lo..=hi).contains(&v) && !out.contains(&v) {
            out.push(v);
        }
    }
    out.into_iter().map(int_value).collect()
}

fn number_edges(s: &Map<String, Value>) -> Vec<Value> {
    let (lo, hi) = number_bounds(s);
    let mut out: Vec<f64> = Vec::new();
    for v in [
        lo,
        hi,
        0.0,
        -0.0,
        0.5,
        -1.5,
        1e-300,
        123_456_789.123,
        1e15 + 0.5,
    ] {
        if v >= lo && v <= hi && v.is_finite() && !out.iter().any(|o| o.to_bits() == v.to_bits()) {
            out.push(v);
        }
    }
    out.into_iter().map(float_value).collect()
}

/// Strings that break naive code but satisfy the schema's lengths.
fn string_edges(s: &Map<String, Value>) -> Vec<Value> {
    let (min, max) = lengths(s);
    let long = max.unwrap_or(LONG_STRING).min(64 * 1024);
    let mut candidates: Vec<String> = vec![
        String::new(),
        "a".into(),
        "a".repeat(long),
        "ü".into(),
        "日本語テキスト".into(),
        "🎉🚀👩‍👩‍👧".into(),
        "e\u{301}\u{301}".into(),
        "\u{202e}desrever".into(),
        "\u{0}".into(),
        "\u{feff}bom".into(),
        " ".into(),
        "\t\r\n".into(),
        "'\"<>&;--".into(),
        "../../etc/passwd".into(),
        "%00%2e%2e".into(),
        "null".into(),
        "0".into(),
        "-1".into(),
        "true".into(),
        "🎉".repeat(long.min(256)),
    ];
    if s.get("format").and_then(Value::as_str).is_some() || s.contains_key("pattern") {
        // A format or pattern makes most of these invalid; keep the lengths.
        candidates.truncate(3);
    }
    let mut out: Vec<Value> = Vec::new();
    for c in candidates {
        let fitted = fit(c, min, max);
        let v = Value::String(fitted);
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// Pad or cut `text` to `min..=max` characters.
fn fit(mut text: String, min: usize, max: Option<usize>) -> String {
    let n = text.chars().count();
    if n < min {
        text.extend(std::iter::repeat_n('x', min - n));
    }
    if let Some(max) = max {
        if text.chars().count() > max {
            text = text.chars().take(max).collect();
        }
    }
    text
}

/// `value` with the location `at` set to `new` (or removed when `None`).
/// Missing parents are created as objects.
pub(crate) fn apply(value: &mut Value, at: &[Seg], new: Option<Value>) {
    let Some((last, parents)) = at.split_last() else {
        if let Some(v) = new {
            *value = v;
        }
        return;
    };
    let mut cur = value;
    for seg in parents {
        cur = match seg {
            Seg::Key(k) => {
                if !cur.is_object() {
                    *cur = Value::Object(Map::new());
                }
                match cur {
                    Value::Object(m) => m.entry(k.clone()).or_insert(Value::Null),
                    _ => return,
                }
            }
            Seg::Index(i) => match cur {
                Value::Array(a) if *i < a.len() => &mut a[*i],
                _ => return,
            },
        };
    }
    match (last, cur) {
        (Seg::Key(k), Value::Object(m)) => match new {
            Some(v) => {
                m.insert(k.clone(), v);
            }
            None => {
                m.remove(k);
            }
        },
        (Seg::Index(i), Value::Array(a)) if *i < a.len() => match new {
            Some(v) => a[*i] = v,
            None => {
                a.remove(*i);
            }
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Value {
        json!({ "components": { "schemas": {
            "Item": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "minLength": 1, "maxLength": 20 },
                    "qty": { "type": "integer", "format": "uint16", "minimum": 0, "maximum": 65535 },
                    "note": { "type": ["string", "null"] },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "shape": { "$ref": "#/components/schemas/Shape" }
                },
                "required": ["name", "qty", "tags", "shape"]
            },
            "Shape": { "oneOf": [
                { "type": "object", "properties": { "kind": { "const": "Circle", "type": "string" }, "r": { "type": "number", "format": "double" } }, "required": ["kind", "r"] },
                { "type": "object", "properties": { "kind": { "const": "Square", "type": "string" }, "side": { "type": "integer", "format": "uint8", "minimum": 0, "maximum": 255 } }, "required": ["kind", "side"] }
            ] }
        } } })
    }

    #[test]
    fn generated_values_validate() {
        let doc = doc();
        let r = Resolver::new(&doc);
        let schema = json!({ "$ref": "#/components/schemas/Item" });
        let mut g = Gen::new(r, 7);
        for mode in [Mode::Typical, Mode::Full, Mode::Random] {
            for _ in 0..200 {
                let v = g.value(&schema, mode);
                assert_eq!(
                    r.validate(&schema, &v),
                    Vec::<String>::new(),
                    "{mode:?}: {v}"
                );
            }
        }
    }

    #[test]
    fn edges_are_valid_and_invalids_are_not() {
        let doc = doc();
        let r = Resolver::new(&doc);
        let mut g = Gen::new(r, 1);
        let base = g.value(&json!({ "$ref": "#/components/schemas/Item" }), Mode::Full);
        let targets = g.targets(&json!({ "$ref": "#/components/schemas/Item" }), &base);
        let pointers: Vec<String> = targets.iter().map(Target::pointer).collect();
        for p in ["", "/name", "/qty", "/note", "/tags", "/tags/0", "/shape"] {
            assert!(
                pointers.iter().any(|x| x == p),
                "{p} missing from {pointers:?}"
            );
        }
        assert!(pointers
            .iter()
            .any(|x| x == "/shape/r" || x == "/shape/side"));
        for t in &targets {
            for (v, label) in g.edges(&t.schema) {
                assert!(r.is_valid(&t.schema, &v), "{} {label}: {v}", t.pointer());
            }
            for (v, label) in g.invalids(&t.schema) {
                assert!(!r.is_valid(&t.schema, &v), "{} {label}: {v}", t.pointer());
            }
        }
        let qty = targets.iter().find(|t| t.pointer() == "/qty").unwrap();
        let edges: Vec<Value> = g.edges(&qty.schema).into_iter().map(|e| e.0).collect();
        assert!(edges.contains(&json!(0)) && edges.contains(&json!(65535)));
        let invalid: Vec<Value> = g.invalids(&qty.schema).into_iter().map(|e| e.0).collect();
        assert!(invalid.contains(&json!(-1)) && invalid.contains(&json!(65536)));
    }

    #[test]
    fn same_seed_same_values() {
        let doc = doc();
        let schema = json!({ "$ref": "#/components/schemas/Item" });
        let run = |seed| {
            let mut g = Gen::new(Resolver::new(&doc), seed);
            (0..20)
                .map(|_| g.value(&schema, Mode::Random))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(3), run(3));
        assert_ne!(run(3), run(4));
    }

    #[test]
    fn apply_sets_and_removes() {
        let mut v = json!({ "a": { "b": [1, 2] } });
        apply(
            &mut v,
            &[Seg::Key("a".into()), Seg::Key("b".into()), Seg::Index(1)],
            Some(json!(9)),
        );
        assert_eq!(v, json!({ "a": { "b": [1, 9] } }));
        apply(&mut v, &[Seg::Key("a".into()), Seg::Key("b".into())], None);
        assert_eq!(v, json!({ "a": {} }));
        apply(&mut v, &[], Some(json!(null)));
        assert_eq!(v, Value::Null);
    }
}
