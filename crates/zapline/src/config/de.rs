//! A lenient serde deserializer over a `toml::Value` tree that records the
//! key path of every error.
//!
//! Leniency matters because environment variables and `.set()` overrides are
//! strings: `"8080"` deserializes as a `u16`, `8080` as a `String`, `"1"` /
//! `"yes"` / `"on"` as `true`, `"a,b"` as a `Vec<String>`, and struct fields
//! and enum variants match case-insensitively when there is no exact match.

use std::fmt;

use serde::de::{
    self, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess, Unexpected,
    VariantAccess, Visitor,
};
use serde::Deserializer;
use toml::{Table, Value};

/// One step in a key path.
#[derive(Debug, Clone)]
pub(crate) enum Seg {
    Key(String),
    Index(usize),
}

/// A deserialization error plus where in the tree it happened.
#[derive(Debug)]
pub(crate) struct DeError {
    path: Vec<Seg>,
    msg: String,
    missing: bool,
}

impl DeError {
    pub(crate) fn missing() -> Self {
        DeError {
            path: Vec::new(),
            msg: "missing".into(),
            missing: true,
        }
    }

    pub(crate) fn is_missing(&self) -> bool {
        self.missing
    }

    pub(crate) fn message(&self) -> &str {
        &self.msg
    }

    /// The key path as `a.b[2].c`.
    pub(crate) fn path(&self) -> String {
        format_path(&self.path)
    }

    fn within(mut self, seg: Seg) -> Self {
        self.path.insert(0, seg);
        self
    }
}

pub(crate) fn format_path(segs: &[Seg]) -> String {
    let mut out = String::new();
    for seg in segs {
        match seg {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Index(i) => {
                out.push('[');
                out.push_str(&i.to_string());
                out.push(']');
            }
        }
    }
    out
}

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.msg)
        } else {
            write!(f, "`{}`: {}", self.path(), self.msg)
        }
    }
}

impl std::error::Error for DeError {}

impl de::Error for DeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DeError {
            path: Vec::new(),
            msg: msg.to_string(),
            missing: false,
        }
    }

    fn missing_field(field: &'static str) -> Self {
        DeError::missing().within(Seg::Key(field.to_owned()))
    }

    // Raised while deserializing a map key; `MapDe` adds the key to the path.
    fn unknown_field(_field: &str, expected: &'static [&'static str]) -> Self {
        DeError::custom(if expected.is_empty() {
            "unknown key (no keys are allowed here)".to_owned()
        } else {
            format!("unknown key, expected one of `{}`", expected.join("`, `"))
        })
    }
}

fn unexpected(v: &Value) -> Unexpected<'_> {
    match v {
        Value::String(s) => Unexpected::Str(s),
        Value::Integer(i) => Unexpected::Signed(*i),
        Value::Float(f) => Unexpected::Float(*f),
        Value::Boolean(b) => Unexpected::Bool(*b),
        Value::Datetime(_) => Unexpected::Other("datetime"),
        Value::Array(_) => Unexpected::Seq,
        Value::Table(_) => Unexpected::Map,
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// A list given as a string: TOML/JSON array syntax, or comma-separated.
fn parse_list(s: &str) -> Result<Vec<Value>, DeError> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    if t.starts_with('[') {
        return match t.parse::<Value>() {
            Ok(Value::Array(items)) => Ok(items),
            _ => Err(de::Error::custom(format!("invalid array syntax: {t:?}"))),
        };
    }
    Ok(t.split(',')
        .map(|item| Value::String(item.trim().to_owned()))
        .collect())
}

/// `name` if it is a known field or variant, else a case-insensitive match.
fn match_name<'a>(name: &'a str, known: &'static [&'static str]) -> &'a str {
    if known.contains(&name) {
        return name;
    }
    known
        .iter()
        .find(|k| k.eq_ignore_ascii_case(name))
        .copied()
        .unwrap_or(name)
}

/// Deserializes a borrowed value.
pub(crate) struct ValueDe<'a>(pub(crate) &'a Value);

macro_rules! integer {
    ($($method:ident => $ty:ty, $visit:ident;)*) => {$(
        fn $method<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            match self.0 {
                Value::Integer(i) => v.visit_i64(*i),
                Value::String(s) => match s.trim().parse::<$ty>() {
                    Ok(n) => v.$visit(n),
                    Err(_) => Err(de::Error::invalid_value(Unexpected::Str(s), &v)),
                },
                _ => self.deserialize_any(v),
            }
        }
    )*};
}

macro_rules! float {
    ($($method:ident => $ty:ty, $visit:ident;)*) => {$(
        fn $method<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            match self.0 {
                Value::Float(f) => v.visit_f64(*f),
                Value::Integer(i) => v.visit_i64(*i),
                Value::String(s) => match s.trim().parse::<$ty>() {
                    Ok(n) => v.$visit(n),
                    Err(_) => Err(de::Error::invalid_value(Unexpected::Str(s), &v)),
                },
                _ => self.deserialize_any(v),
            }
        }
    )*};
}

impl<'de> Deserializer<'de> for ValueDe<'_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::String(s) => v.visit_str(s),
            Value::Integer(i) => v.visit_i64(*i),
            Value::Float(f) => v.visit_f64(*f),
            Value::Boolean(b) => v.visit_bool(*b),
            Value::Datetime(d) => v.visit_string(d.to_string()),
            Value::Array(items) => visit_array(items, v),
            Value::Table(t) => v.visit_map(MapDe::new(t, None)),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::Boolean(b) => v.visit_bool(*b),
            Value::Integer(i @ (0 | 1)) => v.visit_bool(*i == 1),
            Value::String(s) => match parse_bool(s) {
                Some(b) => v.visit_bool(b),
                None => Err(de::Error::invalid_value(Unexpected::Str(s), &v)),
            },
            other => Err(de::Error::invalid_type(unexpected(other), &v)),
        }
    }

    integer! {
        deserialize_i8 => i8, visit_i8;
        deserialize_i16 => i16, visit_i16;
        deserialize_i32 => i32, visit_i32;
        deserialize_i64 => i64, visit_i64;
        deserialize_i128 => i128, visit_i128;
        deserialize_u8 => u8, visit_u8;
        deserialize_u16 => u16, visit_u16;
        deserialize_u32 => u32, visit_u32;
        deserialize_u64 => u64, visit_u64;
        deserialize_u128 => u128, visit_u128;
    }

    float! {
        deserialize_f32 => f32, visit_f32;
        deserialize_f64 => f64, visit_f64;
    }

    fn deserialize_char<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_str(v)
    }

    fn deserialize_str<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::String(s) => v.visit_str(s),
            Value::Integer(i) => v.visit_string(i.to_string()),
            Value::Float(f) => v.visit_string(f.to_string()),
            Value::Boolean(b) => v.visit_str(if *b { "true" } else { "false" }),
            Value::Datetime(d) => v.visit_string(d.to_string()),
            other => Err(de::Error::invalid_type(unexpected(other), &v)),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_str(v)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::String(s) => v.visit_bytes(s.as_bytes()),
            _ => self.deserialize_any(v),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_bytes(v)
    }

    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_some(self)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value, DeError> {
        v.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value, DeError> {
        v.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::Array(items) => visit_array(items, v),
            Value::String(s) => visit_array(&parse_list(s)?, v),
            other => Err(de::Error::invalid_type(unexpected(other), &v)),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, v: V) -> Result<V::Value, DeError> {
        self.deserialize_seq(v)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        v: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(v)
    }

    fn deserialize_map<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::Table(t) => v.visit_map(MapDe::new(t, None)),
            other => Err(de::Error::invalid_type(unexpected(other), &v)),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, DeError> {
        match self.0 {
            Value::Table(t) => v.visit_map(MapDe::new(t, Some(fields))),
            // `toml::value::Datetime` is deserialized through a magic struct.
            Value::Datetime(_) => self
                .0
                .clone()
                .deserialize_struct(name, fields, v)
                .map_err(|e| de::Error::custom(e.message())),
            other => Err(de::Error::invalid_type(unexpected(other), &v)),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, DeError> {
        match self.0 {
            Value::String(s) => v.visit_enum(match_name(s, variants).into_deserializer()),
            Value::Table(t) if t.len() == 1 => {
                let (variant, value) = t.iter().next().expect("one entry");
                v.visit_enum(EnumDe {
                    variant: match_name(variant, variants),
                    value,
                })
            }
            other => Err(de::Error::invalid_type(
                unexpected(other),
                &"a variant name or a table with exactly one key",
            )),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_str(v)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_unit()
    }
}

fn visit_array<'de, V: Visitor<'de>>(items: &[Value], v: V) -> Result<V::Value, DeError> {
    let mut seq = SeqDe {
        iter: items.iter(),
        index: 0,
    };
    let out = v.visit_seq(&mut seq)?;
    if seq.iter.len() == 0 {
        Ok(out)
    } else {
        Err(de::Error::invalid_length(
            items.len(),
            &"fewer elements in the array",
        ))
    }
}

struct SeqDe<'a> {
    iter: std::slice::Iter<'a, Value>,
    index: usize,
}

impl<'de> SeqAccess<'de> for SeqDe<'_> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DeError> {
        let Some(value) = self.iter.next() else {
            return Ok(None);
        };
        let index = self.index;
        self.index += 1;
        seed.deserialize(ValueDe(value))
            .map(Some)
            .map_err(|e| e.within(Seg::Index(index)))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

struct MapDe<'a> {
    iter: toml::map::Iter<'a, String, Value>,
    pending: Option<(&'a str, &'a Value)>,
    fields: Option<&'static [&'static str]>,
}

impl<'a> MapDe<'a> {
    fn new(table: &'a Table, fields: Option<&'static [&'static str]>) -> Self {
        MapDe {
            iter: table.iter(),
            pending: None,
            fields,
        }
    }
}

impl<'de> MapAccess<'de> for MapDe<'_> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, DeError> {
        let Some((key, value)) = self.iter.next() else {
            return Ok(None);
        };
        self.pending = Some((key, value));
        let name = match self.fields {
            Some(fields) => match_name(key, fields),
            None => key,
        };
        // Keys go through `ValueDe` too, so `HashMap<u16, _>` works.
        let key_value = Value::String(name.to_owned());
        seed.deserialize(ValueDe(&key_value))
            .map(Some)
            .map_err(|e| e.within(Seg::Key(key.clone())))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let (key, value) = self
            .pending
            .take()
            .ok_or_else(|| <DeError as de::Error>::custom("value requested before key"))?;
        seed.deserialize(ValueDe(value))
            .map_err(|e| e.within(Seg::Key(key.to_owned())))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

struct EnumDe<'a> {
    variant: &'a str,
    value: &'a Value,
}

impl<'de, 'a> EnumAccess<'de> for EnumDe<'a> {
    type Error = DeError;
    type Variant = VariantDe<'a>;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, VariantDe<'a>), DeError> {
        let name = Value::String(self.variant.to_owned());
        let variant = seed.deserialize(ValueDe(&name))?;
        Ok((
            variant,
            VariantDe {
                variant: self.variant,
                value: self.value,
            },
        ))
    }
}

struct VariantDe<'a> {
    variant: &'a str,
    value: &'a Value,
}

impl VariantDe<'_> {
    fn within(&self, e: DeError) -> DeError {
        e.within(Seg::Key(self.variant.to_owned()))
    }
}

impl<'de> VariantAccess<'de> for VariantDe<'_> {
    type Error = DeError;

    fn unit_variant(self) -> Result<(), DeError> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, DeError> {
        seed.deserialize(ValueDe(self.value))
            .map_err(|e| self.within(e))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, v: V) -> Result<V::Value, DeError> {
        ValueDe(self.value)
            .deserialize_seq(v)
            .map_err(|e| self.within(e))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, DeError> {
        ValueDe(self.value)
            .deserialize_struct("", fields, v)
            .map_err(|e| self.within(e))
    }
}

/// Stands in for a key that does not exist: `Option<T>` becomes `None`,
/// anything else is a "missing" error.
pub(crate) struct MissingDe;

impl<'de> Deserializer<'de> for MissingDe {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, _v: V) -> Result<V::Value, DeError> {
        Err(DeError::missing())
    }

    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_none()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct newtype_struct seq tuple tuple_struct
        map struct enum identifier ignored_any
    }
}
