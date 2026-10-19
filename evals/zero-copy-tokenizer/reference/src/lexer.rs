use crate::token::{Span, Token, TokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexError {
    UnexpectedChar { ch: char, span: Span },
    UnterminatedString { span: Span },
}

pub struct Lexer<'a> {
    src: &'a str,
    /// Byte offset into `src`, always on a char boundary.
    pos: usize,
    line: usize,
    col: usize,
    done: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Lexer<'a> {
        Lexer {
            src,
            pos: 0,
            line: 1,
            col: 1,
            done: false,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
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

    fn take_while(&mut self, pred: impl Fn(char) -> bool) {
        while self.peek().is_some_and(&pred) {
            self.bump();
        }
    }

    fn string(&mut self, span: Span) -> Result<Token<'a>, LexError> {
        self.bump();
        let start = self.pos;
        loop {
            let end = self.pos;
            match self.bump() {
                None => return Err(LexError::UnterminatedString { span }),
                Some('"') => return Ok(Token::new(TokenKind::Str, &self.src[start..end], span)),
                Some('\\') => {
                    if self.bump().is_none() {
                        return Err(LexError::UnterminatedString { span });
                    }
                }
                Some(_) => {}
            }
        }
    }

    fn number(&mut self, span: Span) -> Token<'a> {
        let start = self.pos;
        self.take_while(|c| c.is_ascii_digit());
        if self.peek() == Some('.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
            self.take_while(|c| c.is_ascii_digit());
        }
        Token::new(TokenKind::Number, &self.src[start..self.pos], span)
    }

    fn ident(&mut self, span: Span) -> Token<'a> {
        let start = self.pos;
        self.take_while(|c| c.is_alphanumeric() || c == '_');
        Token::new(TokenKind::Ident, &self.src[start..self.pos], span)
    }

    fn punct(&mut self, kind: TokenKind, span: Span) -> Token<'a> {
        let start = self.pos;
        self.bump();
        Token::new(kind, &self.src[start..self.pos], span)
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<Token<'a>, LexError>;

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

pub fn tokenize(src: &str) -> Result<Vec<Token<'_>>, LexError> {
    Lexer::new(src).collect()
}
