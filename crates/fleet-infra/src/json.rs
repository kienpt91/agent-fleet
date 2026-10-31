//! Conversion between the domain's neutral `Value` and JSON text.

use fleet_domain::value::Value;
use std::collections::BTreeMap;

/// Parses one JSON document. The size is checked before parsing; serde_json's recursion limit
/// stays active, so a deeply nested document is an error rather than a stack overflow.
pub fn parse_value(bytes: &[u8], max_bytes: usize) -> Result<Value, String> {
    if bytes.len() > max_bytes {
        return Err(format!("document is larger than {max_bytes} bytes"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "document is not valid UTF-8".to_string())?;
    let parsed: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        // serde_json messages carry position only, never document content.
        format!("invalid JSON: {e}")
    })?;
    Ok(from_json(&parsed))
}

pub fn to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => {
            if let Ok(i) = i64::try_from(*n) {
                serde_json::Value::from(i)
            } else if let Ok(u) = u64::try_from(*n) {
                serde_json::Value::from(u)
            } else {
                // Outside both integer ranges: nearest float, as JSON has no wider integer.
                serde_json::Number::from_f64(*n as f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
        }
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Array(items) => serde_json::Value::Array(items.iter().map(to_json).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, item) in map {
                out.insert(k.clone(), to_json(item));
            }
            serde_json::Value::Object(out)
        }
    }
}

pub fn from_json(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i128::from(i))
            } else if let Some(u) = n.as_u64() {
                Value::Int(i128::from(u))
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(items) => Value::Array(items.iter().map(from_json).collect()),
        serde_json::Value::Object(map) => {
            let mut out = BTreeMap::new();
            for (k, item) in map {
                out.insert(k.clone(), from_json(item));
            }
            Value::Object(out)
        }
    }
}

/// Compact JSON text without a trailing newline.
pub fn to_line(v: &Value) -> String {
    serde_json::to_string(&to_json(v)).unwrap_or_else(|_| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Value, String> {
        parse_value(s.as_bytes(), 1024 * 1024)
    }

    #[test]
    fn round_trips_nested_documents() {
        let v = Value::object([
            ("a", Value::int(1)),
            (
                "b",
                Value::array([Value::Null, Value::Bool(true), Value::str("x\"\n\u{e9}")]),
            ),
            ("c", Value::object([("d", Value::Float(1.5))])),
        ]);
        let line = to_line(&v);
        assert!(!line.contains('\n'));
        assert_eq!(parse(&line).unwrap(), v);
    }

    #[test]
    fn big_and_negative_numbers_keep_their_value() {
        assert_eq!(
            parse("18446744073709551615").unwrap(),
            Value::Int(i128::from(u64::MAX))
        );
        assert_eq!(
            parse("-9223372036854775808").unwrap(),
            Value::Int(i128::from(i64::MIN))
        );
        assert_eq!(parse("-5").unwrap(), Value::Int(-5));
        assert_eq!(parse("2.5").unwrap(), Value::Float(2.5));
        assert_eq!(
            to_line(&Value::Int(i128::from(u64::MAX))),
            "18446744073709551615"
        );
        assert_eq!(
            to_line(&Value::Int(i128::from(i64::MIN))),
            "-9223372036854775808"
        );
    }

    #[test]
    fn non_finite_floats_and_huge_ints_do_not_panic() {
        assert_eq!(to_line(&Value::Float(f64::NAN)), "null");
        assert_eq!(to_line(&Value::Float(f64::INFINITY)), "null");
        let huge = to_line(&Value::Int(i128::MAX));
        assert!(parse(&huge).is_ok());
    }

    #[test]
    fn rejects_oversize_before_parsing() {
        let err = parse_value(b"[1,2,3]", 3).unwrap_err();
        assert!(err.contains("larger than 3"));
        assert!(parse_value(b"[1,2,3]", 7).is_ok());
    }

    #[test]
    fn rejects_trailing_garbage_and_invalid_utf8() {
        assert!(parse("{} x").is_err());
        assert!(parse("{}{}").is_err());
        assert!(parse("").is_err());
        assert!(parse("{}  \n").is_ok());
        let err = parse_value(&[b'"', 0xff, 0xfe, b'"'], 100).unwrap_err();
        assert!(err.contains("UTF-8"));
    }

    #[test]
    fn a_depth_bomb_is_an_error_not_a_crash() {
        let bomb = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse_value(bomb.as_bytes(), usize::MAX).is_err());
        let objs = format!("{}1{}", "{\"a\":".repeat(5_000), "}".repeat(5_000));
        assert!(parse_value(objs.as_bytes(), usize::MAX).is_err());
    }

    #[test]
    fn duplicate_keys_do_not_panic() {
        assert!(parse(r#"{"a":1,"a":2}"#).is_ok());
    }
}
