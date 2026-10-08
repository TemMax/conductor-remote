//! JavaScript string and JSON semantics the transcript's wire format depends on.
//!
//! The phone app was written against a relay that built these strings in JavaScript, so
//! lengths count UTF-16 code units, trimming uses JavaScript's whitespace set, and a JSON
//! rendering is the text `JSON.stringify` gives.

use serde_json::{Map, Number, Value};

/// Largest key JavaScript treats as an array index (2^32 - 2).
const MAX_ARRAY_INDEX: u64 = 4_294_967_294;

/// Whitespace as `String.prototype.trim` and the regular expression `\s` see it.
///
/// It is not Rust's `char::is_whitespace`: JavaScript counts U+FEFF and does not count U+0085.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// `s.trim()`.
pub(super) fn trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `s.trimStart()`.
pub(super) fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

/// The trimmed string when `value` is a string with something left after trimming.
pub(super) fn non_blank(value: Option<&Value>) -> Option<&str> {
    let trimmed = trim(value?.as_str()?);
    (!trimmed.is_empty()).then_some(trimmed)
}

/// A property of a parsed value, as JavaScript reads `value.key`: only an object has one.
pub(super) fn property<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object()?.get(key)
}

/// Whether JavaScript would take the value for true in a condition.
pub(super) fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// Cuts `s` to `limit` UTF-16 code units and appends an ellipsis; a string within the limit
/// is returned whole.
///
/// When the cut falls between the two code units of one character, JavaScript keeps the
/// first unit, an unpaired surrogate. A Rust string cannot hold one, so U+FFFD stands in
/// for it: the same length, and what a browser draws for an unpaired surrogate.
pub(super) fn clip(s: &str, limit: usize) -> String {
    let mut units = 0;
    for (offset, c) in s.char_indices() {
        let width = c.len_utf16();
        if units + width > limit {
            let mut clipped = String::with_capacity(offset + 6);
            clipped.push_str(&s[..offset]);
            if units < limit {
                clipped.push('\u{FFFD}');
            }
            clipped.push('…');
            return clipped;
        }
        units += width;
    }
    s.to_owned()
}

/// Uppercases the first UTF-16 code unit of `s`, as `s[0].toUpperCase() + s.slice(1)` does.
/// A character outside the Basic Multilingual Plane starts with a surrogate, which has no
/// uppercase form, so it is left as it is.
pub(super) fn uppercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) if first.len_utf16() == 1 => first.to_uppercase().chain(chars).collect(),
        _ => s.to_owned(),
    }
}

/// The text of `JSON.stringify(value)` for a value that came out of `JSON.parse`.
pub(super) fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(out, number),
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in keys_in_js_order(map).into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

/// The order JavaScript enumerates an object's keys in: array indices first, ascending,
/// then every other key in the order the input wrote it.
fn keys_in_js_order(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut indices: Vec<(u64, &String, &Value)> = Vec::new();
    let mut named: Vec<(&String, &Value)> = Vec::new();
    for (key, value) in map {
        match array_index(key) {
            Some(index) => indices.push((index, key, value)),
            None => named.push((key, value)),
        }
    }
    indices.sort_by_key(|(index, _, _)| *index);
    indices
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .chain(named)
        .collect()
}

/// The number a key stands for when JavaScript treats it as an array index: canonical
/// decimal digits, no leading zero, at most 2^32 - 2.
fn array_index(key: &str) -> Option<u64> {
    let canonical = key == "0" || (!key.starts_with('0') && !key.is_empty());
    if !canonical || key.len() > 10 || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse().ok().filter(|index| *index <= MAX_ARRAY_INDEX)
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{0020}' => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Writes a number as JavaScript's `Number::toString` does. Every JSON number is a double
/// there, so an integer too large for one is rounded to the nearest double first.
///
/// The text is exact for the double the JSON reader produced. The reader itself, built
/// without `serde_json`'s `float_roundtrip` feature, can be one step off the nearest double
/// for a literal whose digits do not fit a 64-bit integer or whose exponent is large; the
/// last digit written then differs from JavaScript's.
fn write_number(out: &mut String, number: &Number) {
    let Some(value) = number.as_f64() else {
        out.push_str("null");
        return;
    };
    if !value.is_finite() {
        out.push_str("null");
        return;
    }
    if value == 0.0 {
        out.push('0');
        return;
    }
    if value < 0.0 {
        out.push('-');
    }

    // The shortest digits that read back as the same double, as `d[.ddd]e<exponent>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("the exponent format always writes an exponent");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i32 = exponent
        .parse()
        .expect("the exponent format writes a decimal exponent");
    let count = i32::try_from(digits.len()).expect("a double has at most 17 significant digits");
    // The position of the decimal point, counted from the first digit.
    let point = exponent + 1;

    if count <= point && point <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', usize_of(point - count)));
    } else if 0 < point && point <= 21 {
        let (whole, fraction) = digits.split_at(usize_of(point));
        out.push_str(whole);
        out.push('.');
        out.push_str(fraction);
    } else if -6 < point && point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', usize_of(-point)));
        out.push_str(&digits);
    } else {
        let (first, rest) = digits.split_at(1);
        out.push_str(first);
        if !rest.is_empty() {
            out.push('.');
            out.push_str(rest);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&exponent.unsigned_abs().to_string());
    }
}

fn usize_of(n: i32) -> usize {
    usize::try_from(n).expect("the caller checked the value is not negative")
}

/// `JSON.parse(text)`, or `None` where it would throw.
///
/// JavaScript accepts an escaped unpaired surrogate, which `serde_json` refuses; such an
/// escape is read as U+FFFD instead, the stand-in `clip` uses.
pub(super) fn parse(text: &str) -> Option<Value> {
    serde_json::from_str(text)
        .ok()
        .or_else(|| serde_json::from_str(&replace_lone_surrogates(text)).ok())
}

/// The `\uXXXX` escape that starts at `at`, as its code unit.
fn unicode_escape(bytes: &[u8], at: usize) -> Option<u16> {
    let digits = bytes.get(at..at + 6)?;
    if digits[0] != b'\\' || digits[1] != b'u' {
        return None;
    }
    let hex = std::str::from_utf8(&digits[2..]).ok()?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(hex, 16).ok()
}

/// Replaces each escaped unpaired surrogate (a high surrogate not directly followed by a
/// low one, a low surrogate not directly preceded by a high one) with `�`. An escaped
/// backslash is skipped whole, so `\\ud83d` is text.
fn replace_lone_surrogates(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        match unicode_escape(bytes, i) {
            Some(0xD800..=0xDBFF)
                if matches!(unicode_escape(bytes, i + 6), Some(0xDC00..=0xDFFF)) =>
            {
                i += 12;
            }
            Some(0xD800..=0xDFFF) => {
                out.push_str(&text[copied..i]);
                out.push_str("\\ufffd");
                i += 6;
                copied = i;
            }
            Some(_) => i += 6,
            None => i += 2,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// `String(value)` for a value that came out of `JSON.parse`.
pub(super) fn to_js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => {
            let mut out = String::new();
            write_number(&mut out, number);
            out
        }
        Value::String(text) => text.clone(),
        // `Array.prototype.join`: null items are empty, the rest are converted in turn.
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                item => to_js_string(item),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// What a `JSON.stringify` replacer returns for one key.
pub(super) enum Replacement {
    /// The value it was given.
    Keep,
    /// `undefined`: the key is left out of an object, and an array item is written `null`.
    Omit,
    /// Another value, written in its place (and itself passed through the replacer).
    With(Value),
}

/// A replacer: called as `replacer.call(holder, key, value)`. `holder` is the object that
/// holds the key, or `None` for an array or the root's wrapper, whose properties a replacer
/// only reads by name and which have none it could ask for.
pub(super) type Replacer<'r> =
    dyn FnMut(Option<&Map<String, Value>>, &str, &Value) -> Replacement + 'r;

/// The text of `JSON.stringify(value, replacer)`; `None` where it returns `undefined`.
pub(super) fn stringify_with(value: &Value, replacer: &mut Replacer<'_>) -> Option<String> {
    let mut out = String::new();
    match replacer(None, "", value) {
        Replacement::Keep => write_replaced(&mut out, value, replacer),
        Replacement::Omit => return None,
        Replacement::With(other) => write_replaced(&mut out, &other, replacer),
    }
    Some(out)
}

fn write_replaced(out: &mut String, value: &Value, replacer: &mut Replacer<'_>) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                match replacer(None, &index.to_string(), item) {
                    Replacement::Keep => write_replaced(out, item, replacer),
                    Replacement::Omit => out.push_str("null"),
                    Replacement::With(other) => write_replaced(out, &other, replacer),
                }
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut first = true;
            for (key, item) in keys_in_js_order(map) {
                let replaced = match replacer(Some(map), key, item) {
                    Replacement::Keep => None,
                    Replacement::Omit => continue,
                    Replacement::With(other) => Some(other),
                };
                if !first {
                    out.push(',');
                }
                first = false;
                write_string(out, key);
                out.push(':');
                write_replaced(out, replaced.as_ref().unwrap_or(item), replacer);
            }
            out.push('}');
        }
        scalar => write_value(out, scalar),
    }
}
