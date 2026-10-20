use crate::token::{Span, Token, TokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexError {
    UnexpectedChar { ch: char, span: Span },
    UnterminatedString { span: Span },
}

pub struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
    done: bool,
}

impl Lexer {
    pub fn new(src: &str) -> Lexer {
        Lexer {
            chars: src.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
            done: false,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn skip_trivia(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.bump();
            } else if c == '#' {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.bump();
                }
            } else {
                break;
            }
        }
    }

    fn take_while(&mut self, text: &mut String, pred: impl Fn(char) -> bool) {
        while let Some(c) = self.peek() {
            if !pred(c) {
                break;
            }
            text.push(c);
            self.bump();
        }
    }

    fn string(&mut self, span: Span) -> Result<Token, LexError> {
        self.bump();
        let mut text = String::new();
        loop {
            match self.bump() {
                None => return Err(LexError::UnterminatedString { span }),
                Some('"') => return Ok(Token::new(TokenKind::Str, &text, span)),
                Some('\\') => {
                    text.push('\\');
                    match self.bump() {
                        Some(c) => text.push(c),
                        None => return Err(LexError::UnterminatedString { span }),
                    }
                }
                Some(c) => text.push(c),
            }
        }
    }

    fn number(&mut self, span: Span) -> Token {
        let mut text = String::new();
        self.take_while(&mut text, |c| c.is_ascii_digit());
        if self.peek() == Some('.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
            text.push('.');
            self.bump();
            self.take_while(&mut text, |c| c.is_ascii_digit());
        }
        Token::new(TokenKind::Number, &text, span)
    }

    fn ident(&mut self, span: Span) -> Token {
        let mut text = String::new();
        self.take_while(&mut text, |c| c.is_alphanumeric() || c == '_');
        Token::new(TokenKind::Ident, &text, span)
    }

    fn punct(&mut self, kind: TokenKind, span: Span) -> Token {
        let mut text = String::new();
        if let Some(c) = self.bump() {
            text.push(c);
        }
        Token::new(kind, &text, span)
    }
}

impl Iterator for Lexer {
    type Item = Result<Token, LexError>;

    /// Yields tokens in order. After the first error it yields nothing more.
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.skip_trivia();
        let span = Span {
            line: self.line,
            col: self.col,
        };
        let result = match self.peek()? {
            '"' => self.string(span),
            c if c.is_ascii_digit() => Ok(self.number(span)),
            c if c.is_alphabetic() || c == '_' => Ok(self.ident(span)),
            '=' => Ok(self.punct(TokenKind::Equals, span)),
            ';' => Ok(self.punct(TokenKind::Semicolon, span)),
            ',' => Ok(self.punct(TokenKind::Comma, span)),
            '[' => Ok(self.punct(TokenKind::LBracket, span)),
            ']' => Ok(self.punct(TokenKind::RBracket, span)),
            ch => Err(LexError::UnexpectedChar { ch, span }),
        };
        self.done = result.is_err();
        Some(result)
    }
}

pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(src).collect()
}
