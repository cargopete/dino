use std::collections::BTreeMap;

pub type Table = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Integer(i64),
    Boolean(bool),
    Array(Vec<Value>),
    Table(Table),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Syntax,
    Escape,
    Integer,
    Redefinition,
}

/// Why `parse` failed. `line` is 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub kind: ErrorKind,
}

pub fn parse(src: &str) -> Result<Table, ParseError> {
    let _ = src;
    todo!()
}
