#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Ident,
    Number,
    Str,
    Equals,
    Semicolon,
    Comma,
    LBracket,
    RBracket,
}

/// Where a token starts: 1-based line, and 1-based column counted in chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// For a string literal, what lies between the quotes, escapes as written.
    pub text: String,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, text: &str, span: Span) -> Token {
        Token {
            kind,
            text: text.to_string(),
            span,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is(&self, kind: TokenKind) -> bool {
        self.kind == kind
    }
}
