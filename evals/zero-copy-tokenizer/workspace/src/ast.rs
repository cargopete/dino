use crate::token::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Ident(String),
    Number(String),
    /// Raw, escapes as written. `query::unescape` decodes it.
    Str(String),
    List(Vec<Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Ident(s) | Value::Number(s) | Value::Str(s) => Some(s),
            Value::List(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub key_span: Span,
    pub value: Value,
}
