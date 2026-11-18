//! JS `Number(x)` on a JSON config value, so config handling matches the JS
//! engine exactly (zrtcConfig fields arrive as numbers, strings or null).

pub use serde_json::Value;

/// `Number(v)`; a missing key is `undefined` (NaN).
pub fn num(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => *b as u8 as f64,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => str_num(s),
        Some(Value::Array(a)) => match a.len() {
            0 => 0.0,
            1 => match &a[0] {
                Value::Array(_) | Value::Object(_) => f64::NAN,
                Value::Null => 0.0,
                Value::String(s) => str_num(s),
                other => num(Some(other)),
            },
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

fn str_num(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    match t {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') => f64::NAN,
        _ => t.parse().unwrap_or(f64::NAN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn like_js_number() {
        assert!(num(None).is_nan());
        assert_eq!(num(Some(&json!(null))), 0.0);
        assert_eq!(num(Some(&json!("12"))), 12.0);
        assert_eq!(num(Some(&json!(" 7.5 "))), 7.5);
        assert_eq!(num(Some(&json!(""))), 0.0);
        assert!(num(Some(&json!("abc"))).is_nan());
        assert_eq!(num(Some(&json!(true))), 1.0);
        assert_eq!(num(Some(&json!([]))), 0.0);
        assert_eq!(num(Some(&json!(["4"]))), 4.0);
        assert!(num(Some(&json!({}))).is_nan());
    }
}
