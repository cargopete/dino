use crate::ast::{Entry, Value};
use crate::lexer::{tokenize, LexError};
use crate::token::{Span, Token, TokenKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Expected { expected: &'static str, found: Token },
    UnexpectedEof { expected: &'static str },
    DuplicateKey { key: String, first: Span, second: Span },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Lex(LexError),
    Parse(ParseError),
}

struct Parser<'t> {
    tokens: &'t [Token],
    pos: usize,
}

impl<'t> Parser<'t> {
    fn peek(&self) -> Option<&'t Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self, expected: &'static str) -> Result<&'t Token, ParseError> {
        let tok = self
            .tokens
            .get(self.pos)
            .ok_or(ParseError::UnexpectedEof { expected })?;
        self.pos += 1;
        Ok(tok)
    }

    fn expect(&mut self, kind: TokenKind, expected: &'static str) -> Result<&'t Token, ParseError> {
        let tok = self.next(expected)?;
        if !tok.is(kind) {
            return Err(ParseError::Expected {
                expected,
                found: tok.clone(),
            });
        }
        Ok(tok)
    }

    fn entry(&mut self) -> Result<Entry, ParseError> {
        let key = self.expect(TokenKind::Ident, "a key")?;
        self.expect(TokenKind::Equals, "`=`")?;
        let value = self.value()?;
        self.expect(TokenKind::Semicolon, "`;`")?;
        Ok(Entry {
            key: key.text.clone(),
            key_span: key.span,
            value,
        })
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        let tok = self.next("a value")?;
        match tok.kind {
            TokenKind::Ident => Ok(Value::Ident(tok.text.clone())),
            TokenKind::Number => Ok(Value::Number(tok.text.clone())),
            TokenKind::Str => Ok(Value::Str(tok.text.clone())),
            TokenKind::LBracket => self.list(),
            _ => Err(ParseError::Expected {
                expected: "a value",
                found: tok.clone(),
            }),
        }
    }

    /// The opening bracket is already consumed. A trailing comma is allowed.
    fn list(&mut self) -> Result<Value, ParseError> {
        let mut items = Vec::new();
        loop {
            if self.peek().is_some_and(|t| t.is(TokenKind::RBracket)) {
                self.pos += 1;
                return Ok(Value::List(items));
            }
            items.push(self.value()?);
            let tok = self.next("`,` or `]`")?;
            match tok.kind {
                TokenKind::Comma => {}
                TokenKind::RBracket => return Ok(Value::List(items)),
                _ => {
                    return Err(ParseError::Expected {
                        expected: "`,` or `]`",
                        found: tok.clone(),
                    })
                }
            }
        }
    }
}

pub fn parse(tokens: &[Token]) -> Result<Vec<Entry>, ParseError> {
    let mut parser = Parser { tokens, pos: 0 };
    let mut entries: Vec<Entry> = Vec::new();
    while parser.peek().is_some() {
        let entry = parser.entry()?;
        if let Some(first) = entries.iter().find(|e| e.key == entry.key) {
            return Err(ParseError::DuplicateKey {
                key: entry.key.clone(),
                first: first.key_span,
                second: entry.key_span,
            });
        }
        entries.push(entry);
    }
    Ok(entries)
}

pub fn parse_str(src: &str) -> Result<Vec<Entry>, Error> {
    let tokens = tokenize(src).map_err(Error::Lex)?;
    parse(&tokens).map_err(Error::Parse)
}
