#![allow(dead_code)]
use fixture::{parse, ErrorKind, ParseError, Table, Value};

pub fn s(text: &str) -> Value {
    Value::String(text.to_owned())
}

pub fn i(value: i64) -> Value {
    Value::Integer(value)
}

pub fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

pub fn table(entries: Vec<(&str, Value)>) -> Value {
    Value::Table(entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

pub fn doc(entries: Vec<(&str, Value)>) -> Table {
    entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

pub fn ok(src: &str) -> Table {
    match parse(src) {
        Ok(table) => table,
        Err(error) => panic!("{src:?} should parse, got {error:?}"),
    }
}

pub fn value(src: &str) -> Value {
    ok(&format!("v = {src}\n")).remove("v").unwrap()
}

#[track_caller]
pub fn fails(src: &str, line: usize, kind: ErrorKind) {
    assert_eq!(parse(src), Err(ParseError { line, kind }), "input: {src:?}");
}
