//! A small JSON-shaped data value.
//!
//! The domain never parses or prints JSON text (that is an adapter concern) but it does define
//! stable data documents: domain events, status views and the shapes of worker files. `Value`
//! is the neutral representation adapters convert to and from. Objects keep their keys sorted, so
//! [`Value::canonical`] is deterministic and can be hashed.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Every JSON integer that fits `i64` or `u64`.
    Int(i128),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    pub fn int(n: impl Into<i128>) -> Value {
        Value::Int(n.into())
    }

    pub fn object<K: Into<String>, I: IntoIterator<Item = (K, Value)>>(pairs: I) -> Value {
        Value::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn array<I: IntoIterator<Item = Value>>(items: I) -> Value {
        Value::Array(items.into_iter().collect())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => i64::try_from(*n).ok(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Int(n) => u64::try_from(*n).ok(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Deterministic JSON text: sorted keys, no whitespace, minimal escaping.
    pub fn canonical(&self) -> String {
        let mut out = String::new();
        self.write_canonical(&mut out);
        out
    }

    fn write_canonical(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(f) => {
                if f.is_finite() {
                    let _ = write!(out, "{f:?}");
                } else {
                    out.push_str("null");
                }
            }
            Value::Str(s) => write_json_string(s, out),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_canonical(out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_canonical(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Conversion between domain data and [`Value`]. Decoding validates: it returns `None` for
/// anything that does not fit the type, so a damaged or hand-edited record is rejected instead
/// of being half applied.
pub trait Field: Sized {
    fn to_value(&self) -> Value;
    fn from_value(v: &Value) -> Option<Self>;
}

impl Field for String {
    fn to_value(&self) -> Value {
        Value::Str(self.clone())
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_str().map(str::to_string)
    }
}

impl Field for bool {
    fn to_value(&self) -> Value {
        Value::Bool(*self)
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_bool()
    }
}

impl Field for u64 {
    fn to_value(&self) -> Value {
        Value::Int(i128::from(*self))
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_u64()
    }
}

impl Field for u32 {
    fn to_value(&self) -> Value {
        Value::Int(i128::from(*self))
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_u64().and_then(|n| u32::try_from(n).ok())
    }
}

impl Field for i64 {
    fn to_value(&self) -> Value {
        Value::Int(i128::from(*self))
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_i64()
    }
}

impl<T: Field> Field for Option<T> {
    fn to_value(&self) -> Value {
        match self {
            Some(v) => v.to_value(),
            None => Value::Null,
        }
    }
    fn from_value(v: &Value) -> Option<Self> {
        if v.is_null() {
            Some(None)
        } else {
            T::from_value(v).map(Some)
        }
    }
}

impl<T: Field> Field for Vec<T> {
    fn to_value(&self) -> Value {
        Value::Array(self.iter().map(Field::to_value).collect())
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_array()?.iter().map(T::from_value).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sorts_keys_and_escapes() {
        let v = Value::object([
            ("b", Value::int(2)),
            ("a", Value::str("x\"y\n\u{1}")),
            ("c", Value::array([Value::Null, Value::Bool(true)])),
        ]);
        assert_eq!(
            v.canonical(),
            r#"{"a":"x\"y\n\u0001","b":2,"c":[null,true]}"#
        );
    }

    #[test]
    fn canonical_is_independent_of_insertion_order() {
        let a = Value::object([("x", Value::int(1)), ("y", Value::int(2))]);
        let b = Value::object([("y", Value::int(2)), ("x", Value::int(1))]);
        assert_eq!(a.canonical(), b.canonical());
        assert_eq!(a, b);
    }

    #[test]
    fn numbers_round_trip_through_accessors() {
        assert_eq!(Value::int(5u64).as_u64(), Some(5));
        assert_eq!(Value::int(-5i64).as_u64(), None);
        assert_eq!(Value::int(-5i64).as_i64(), Some(-5));
        assert_eq!(Value::Int(i128::from(u64::MAX)).as_i64(), None);
        assert_eq!(Value::Float(1.5).as_u64(), None);
    }

    #[test]
    fn non_finite_floats_do_not_poison_the_canonical_form() {
        assert_eq!(Value::Float(f64::NAN).canonical(), "null");
        assert_eq!(Value::Float(0.5).canonical(), "0.5");
    }

    #[test]
    fn field_decoding_rejects_wrong_types() {
        assert_eq!(String::from_value(&Value::int(1)), None);
        assert_eq!(u32::from_value(&Value::int(u64::MAX)), None);
        assert_eq!(bool::from_value(&Value::str("true")), None);
        assert_eq!(
            Vec::<String>::from_value(&Value::array([Value::int(1)])),
            None
        );
        assert_eq!(
            Option::<String>::from_value(&Value::Null),
            Some(None::<String>)
        );
        assert_eq!(
            Option::<String>::from_value(&Value::str("a")),
            Some(Some("a".to_string()))
        );
        assert_eq!(Option::<u64>::from_value(&Value::str("a")), None);
    }

    #[test]
    fn field_round_trip() {
        let v = vec!["a".to_string(), "b".to_string()];
        assert_eq!(Vec::<String>::from_value(&v.to_value()), Some(v));
        assert_eq!(
            Option::<u64>::from_value(&Some(7u64).to_value()),
            Some(Some(7))
        );
        assert_eq!(i64::from_value(&(-3i64).to_value()), Some(-3));
    }

    #[test]
    fn object_accessors() {
        let v = Value::object([("k", Value::str("v"))]);
        assert_eq!(v.get("k").and_then(Value::as_str), Some("v"));
        assert!(v.get("missing").is_none());
        assert!(Value::Null.get("k").is_none());
        assert!(v.as_object().is_some());
        assert!(Value::Null.is_null());
    }
}
