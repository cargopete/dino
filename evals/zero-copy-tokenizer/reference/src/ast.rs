use crate::token::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    Ident(&'a str),
    Number(&'a str),
    /// Raw, escapes as written. `query::unescape` decodes it.
    Str(&'a str),
    List(Vec<Value<'a>>),
}

impl<'a> Value<'a> {
    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            Value::Ident(s) | Value::Number(s) | Value::Str(s) => Some(s),
            Value::List(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<'a> {
    pub key: &'a str,
    pub key_span: Span,
    pub value: Value<'a>,
}
