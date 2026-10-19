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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokenKind,
    /// For a string literal, what lies between the quotes, escapes as written.
    pub text: &'a str,
    pub span: Span,
}

impl<'a> Token<'a> {
    pub fn new(kind: TokenKind, text: &'a str, span: Span) -> Token<'a> {
        Token { kind, text, span }
    }

    pub fn as_str(&self) -> &'a str {
        self.text
    }

    pub fn is(&self, kind: TokenKind) -> bool {
        self.kind == kind
    }
}
