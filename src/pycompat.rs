//! The few CPython and orjson behaviours the identity layer reproduces so that
//! the same inputs hash to the same identity in every launcher: orjson's
//! sorted-key JSON bytes (hashed by `identity_salt`), `str()` of a value
//! (hashed by `identity_seed`) and `zlib.crc32`.

use serde_json::Value;

/// `repr(float)` in CPython.
pub fn py_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let (digits, exp) = shortest_digits(x.abs());
    if (-4..16).contains(&exp) {
        format!("{sign}{}", fixed_notation(&digits, exp))
    } else {
        let e = format!("{:02}", exp.abs());
        format!("{sign}{}e{}{e}", sci_mantissa(&digits), if exp < 0 { "-" } else { "+" })
    }
}

/// A float exactly as orjson serializes it.
pub fn orjson_float(x: f64) -> String {
    if !x.is_finite() {
        return "null".into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let (digits, exp) = shortest_digits(x.abs());
    if (-5..16).contains(&exp) {
        format!("{sign}{}", fixed_notation(&digits, exp))
    } else {
        format!("{sign}{}e{}{}", sci_mantissa(&digits), if exp < 0 { "-" } else { "+" }, exp.abs())
    }
}

/// Shortest round-trip digits and decimal exponent of a finite, positive x.
fn shortest_digits(x: f64) -> (String, i32) {
    // Rust's `{:e}` prints the shortest representation that round-trips.
    let s = format!("{:e}", x);
    let (mantissa, exponent) = s.split_once('e').unwrap();
    let digits = mantissa.replace('.', "");
    (digits, exponent.parse().unwrap())
}

fn fixed_notation(digits: &str, exp: i32) -> String {
    if exp >= 0 {
        let e = exp as usize;
        if digits.len() <= e + 1 {
            format!("{digits}{}.0", "0".repeat(e + 1 - digits.len()))
        } else {
            format!("{}.{}", &digits[..e + 1], &digits[e + 1..])
        }
    } else {
        format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
    }
}

fn sci_mantissa(digits: &str) -> String {
    if digits.len() == 1 {
        digits.to_string()
    } else {
        format!("{}.{}", &digits[..1], &digits[1..])
    }
}

/// `str()` of a JSON-like value, as the identity seed hashes it.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                py_float_repr(n.as_f64().unwrap_or(0.0))
            }
        }
        other => py_repr(other),
    }
}

/// `repr()` of a JSON-like value.
pub fn py_repr(value: &Value) -> String {
    match value {
        Value::String(s) => py_str_repr(s),
        Value::Array(items) => {
            format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", "))
        }
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter().map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v))).collect::<Vec<_>>().join(", ")
        ),
        other => py_str(other),
    }
}

fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for ch in s.chars() {
        let cp = ch as u32;
        if ch == quote || ch == '\\' {
            out.push('\\');
            out.push(ch);
        } else if ch == '\t' {
            out.push_str("\\t");
        } else if ch == '\n' {
            out.push_str("\\n");
        } else if ch == '\r' {
            out.push_str("\\r");
        } else if cp < 0x20 || cp == 0x7f {
            out.push_str(&format!("\\x{cp:02x}"));
        } else {
            out.push(ch);
        }
    }
    out.push(quote);
    out
}

fn orjson_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `orjson.dumps(value, option=OPT_SORT_KEYS)`: compact, keys sorted by code
/// point, floats in orjson's format.
pub fn orjson_dumps_sorted(value: &Value) -> String {
    let mut out = String::new();
    write_sorted(value, &mut out);
    out
}

fn write_sorted(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else {
                out.push_str(&orjson_float(n.as_f64().unwrap_or(0.0)));
            }
        }
        Value::String(s) => orjson_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_sorted(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                orjson_string(key, out);
                out.push(':');
                write_sorted(&map[*key], out);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(value, ensure_ascii=True)` with compact separators: every
/// non-ASCII character escaped, so the text survives an ANSI code page.
pub fn json_dumps_ascii(value: &Value) -> String {
    let compact = serde_json::to_string(value).unwrap_or_default();
    let mut out = String::with_capacity(compact.len());
    for ch in compact.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{:04x}", unit));
            }
        }
    }
    out
}

/// `zlib.crc32(data) & 0xFFFFFFFF`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr() {
        assert_eq!(py_float_repr(1.0), "1.0");
        assert_eq!(py_float_repr(1.5), "1.5");
        assert_eq!(py_float_repr(1e16), "1e+16");
        assert_eq!(py_float_repr(0.0001), "0.0001");
        assert_eq!(py_float_repr(0.00001), "1e-05");
        assert_eq!(orjson_float(0.00001), "0.00001");
        assert_eq!(orjson_float(0.000001), "1e-6");
    }

    #[test]
    fn sorted_dumps() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":[1.0,"x"],"c":{"z":null,"y":true}}"#).unwrap();
        assert_eq!(orjson_dumps_sorted(&v), r#"{"a":[1.0,"x"],"b":1,"c":{"y":true,"z":null}}"#);
    }

    #[test]
    fn ascii_dumps() {
        let v = serde_json::json!({"a": "é😀"});
        assert_eq!(json_dumps_ascii(&v), r#"{"a":"\u00e9\ud83d\ude00"}"#);
    }
}
