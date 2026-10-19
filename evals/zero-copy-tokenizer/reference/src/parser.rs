use crate::ast::{Entry, Value};
use crate::lexer::{tokenize, LexError};
use crate::token::{Span, Token, TokenKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError<'a> {
    Expected { expected: &'static str, found: Token<'a> },
    UnexpectedEof { expected: &'static str },
    DuplicateKey { key: &'a str, first: Span, second: Span },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error<'a> {
    Lex(LexError),
    Parse(ParseError<'a>),
}

struct Parser<'t, 'a> {
    tokens: &'t [Token<'a>],
    pos: usize,
}

impl<'t, 'a> Parser<'t, 'a> {
    fn peek(&self) -> Option<&'t Token<'a>> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self, expected: &'static str) -> Result<&'t Token<'a>, ParseError<'a>> {
        let tok = self
            .tokens
            .get(self.pos)
            .ok_or(ParseError::UnexpectedEof { expected })?;
        self.pos += 1;
        Ok(tok)
    }

    fn expect(
        &mut self,
        kind: TokenKind,
        expected: &'static str,
    ) -> Result<&'t Token<'a>, ParseError<'a>> {
        let tok = self.next(expected)?;
        if !tok.is(kind) {
            return Err(ParseError::Expected {
                expected,
                found: *tok,
            });
        }
        Ok(tok)
    }

    fn entry(&mut self) -> Result<Entry<'a>, ParseError<'a>> {
        let key = self.expect(TokenKind::Ident, "a key")?;
        self.expect(TokenKind::Equals, "`=`")?;
        let value = self.value()?;
        self.expect(TokenKind::Semicolon, "`;`")?;
        Ok(Entry {
            key: key.text,
            key_span: key.span,
            value,
        })
    }

    fn value(&mut self) -> Result<Value<'a>, ParseError<'a>> {
        let tok = self.next("a value")?;
        match tok.kind {
            TokenKind::Ident => Ok(Value::Ident(tok.text)),
            TokenKind::Number => Ok(Value::Number(tok.text)),
            TokenKind::Str => Ok(Value::Str(tok.text)),
            TokenKind::LBracket => self.list(),
            _ => Err(ParseError::Expected {
                expected: "a value",
                found: *tok,
            }),
        }
    }

    /// The opening bracket is already consumed. A trailing comma is allowed.
    fn list(&mut self) -> Result<Value<'a>, ParseError<'a>> {
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
                        found: *tok,
                    })
                }
            }
        }
    }
}

pub fn parse<'a>(tokens: &[Token<'a>]) -> Result<Vec<Entry<'a>>, ParseError<'a>> {
    let mut parser = Parser { tokens, pos: 0 };
    let mut entries: Vec<Entry<'a>> = Vec::new();
    while parser.peek().is_some() {
        let entry = parser.entry()?;
        if let Some(first) = entries.iter().find(|e| e.key == entry.key) {
            return Err(ParseError::DuplicateKey {
                key: entry.key,
                first: first.key_span,
                second: entry.key_span,
            });
        }
        entries.push(entry);
    }
    Ok(entries)
}

pub fn parse_str(src: &str) -> Result<Vec<Entry<'_>>, Error<'_>> {
    let tokens = tokenize(src).map_err(Error::Lex)?;
    parse(&tokens).map_err(Error::Parse)
}
