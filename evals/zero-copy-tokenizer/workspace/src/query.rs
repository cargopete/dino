use crate::ast::{Entry, Value};

pub fn get<'e>(entries: &'e [Entry], key: &str) -> Option<&'e Value> {
    entries.iter().find(|e| e.key == key).map(|e| &e.value)
}

pub fn keys(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.key.as_str()).collect()
}

/// The decoded text of a string value, or `None` if `key` is missing or not a string.
pub fn get_str(entries: &[Entry], key: &str) -> Option<String> {
    match get(entries, key)? {
        Value::Str(raw) => Some(unescape(raw)),
        _ => None,
    }
}

/// Decodes `\n`, `\t`, `\\` and `\"`. Any other escaped character stands for itself.
pub fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}
