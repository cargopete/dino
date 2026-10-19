// The lexer as it was before the refactor, kept here as the oracle.
mod old {
    use fixture::{LexError, Span, TokenKind};

    #[derive(Debug, PartialEq)]
    pub struct Token {
        pub kind: TokenKind,
        pub text: String,
        pub span: Span,
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
            Lexer { chars: src.chars().collect(), pos: 0, line: 1, col: 1, done: false }
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

        fn take_while(&mut self, text: &mut String, pred: impl Fn(char) -> bool) {
            while let Some(c) = self.peek() {
                if !pred(c) {
                    break;
                }
                text.push(c);
                self.bump();
            }
        }

        fn token(&mut self, span: Span) -> Result<Token, LexError> {
            let mut text = String::new();
            let c = self.peek().unwrap();
            let kind = match c {
                '"' => {
                    self.bump();
                    loop {
                        match self.bump() {
                            None => return Err(LexError::UnterminatedString { span }),
                            Some('"') => break,
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
                    TokenKind::Str
                }
                c if c.is_ascii_digit() => {
                    self.take_while(&mut text, |c| c.is_ascii_digit());
                    if self.peek() == Some('.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
                        text.push('.');
                        self.bump();
                        self.take_while(&mut text, |c| c.is_ascii_digit());
                    }
                    TokenKind::Number
                }
                c if c.is_alphabetic() || c == '_' => {
                    self.take_while(&mut text, |c| c.is_alphanumeric() || c == '_');
                    TokenKind::Ident
                }
                '=' | ';' | ',' | '[' | ']' => {
                    text.push(c);
                    self.bump();
                    match c {
                        '=' => TokenKind::Equals,
                        ';' => TokenKind::Semicolon,
                        ',' => TokenKind::Comma,
                        '[' => TokenKind::LBracket,
                        _ => TokenKind::RBracket,
                    }
                }
                ch => return Err(LexError::UnexpectedChar { ch, span }),
            };
            Ok(Token { kind, text, span })
        }
    }

    impl Iterator for Lexer {
        type Item = Result<Token, LexError>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.done {
                return None;
            }
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
            self.peek()?;
            let span = Span { line: self.line, col: self.col };
            let result = self.token(span);
            self.done = result.is_err();
            Some(result)
        }
    }
}

use fixture::{tokenize, LexError, Lexer, Span, TokenKind};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

fn byte_offset(src: &str, span: Span) -> usize {
    let (mut line, mut col) = (1, 1);
    for (i, c) in src.char_indices() {
        if (line, col) == (span.line, span.col) {
            return i;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    panic!("span {span:?} is not in {src:?}");
}

type Lexed<'s> = Vec<Result<(TokenKind, &'s str, Span), LexError>>;

fn check(src: &str) {
    let want: Vec<Result<(TokenKind, String, Span), LexError>> = old::Lexer::new(src)
        .map(|r| r.map(|t| (t.kind, t.text, t.span)))
        .collect();
    let got: Lexed = Lexer::new(src).map(|r| r.map(|t| (t.kind, t.text, t.span))).collect();
    let got_owned: Vec<Result<(TokenKind, String, Span), LexError>> = got
        .iter()
        .map(|r| r.map(|(k, s, sp)| (k, s.to_owned(), sp)))
        .collect();
    assert_eq!(got_owned, want, "source: {src:?}");

    let base = src.as_ptr() as usize;
    for (kind, text, span) in got.iter().flatten() {
        let quote = (*kind == TokenKind::Str) as usize;
        let at = text.as_ptr() as usize;
        assert!(
            at >= base && at + text.len() <= base + src.len(),
            "{text:?} does not point into {src:?}"
        );
        assert_eq!(at - base, byte_offset(src, *span) + quote, "{text:?} is not the slice at {span:?} of {src:?}");
    }

    let all: Result<Vec<_>, _> = want.into_iter().collect();
    let tokenized = tokenize(src).map(|v| v.iter().map(|t| (t.kind, t.text.to_owned(), t.span)).collect::<Vec<_>>());
    assert_eq!(tokenized, all, "source: {src:?}");
}

#[test]
fn tricky_inputs() {
    let cases = [
        "",
        "   \n\t ",
        "# only a comment",
        "#",
        "a#b\nc",
        "s = \"a\\\"b\\\\\";",
        "s = \"\\é\" t",
        "s = \"\\😀x\";",
        "s = \"\\",
        "s = \"abc\\\"",
        "s = \"multi\nline\" next",
        "größe = 1;",
        "日本 = \"語\";\n名前 = 2;",
        "a٣ = 1;",
        "٣",
        "x = 1.5.6",
        "x = 1..2",
        "x = 12ab",
        "x = 1.é",
        "x = 1.",
        "a\r\nb\rc",
        "a\u{3000}b\u{a0}c",
        "# ünï\nx",
        "日本 $",
        "e\u{301}",
        "_ = __a_b;",
        "a $ b",
        "[[],[x,],];",
    ];
    for src in cases {
        check(src);
    }
}

#[test]
fn lexer_stops_after_an_error() {
    let items: Vec<_> = Lexer::new("a $ b").collect();
    assert_eq!(items.len(), 2);
    assert!(items[0].is_ok());
    assert_eq!(items[1], Err(LexError::UnexpectedChar { ch: '$', span: Span { line: 1, col: 3 } }));
}

#[test]
fn random_character_soup() {
    let alphabet = [
        "a", "z", "Q", "_", "é", "ß", "日", "٣", "0", "7", ".", "\"", "\"", "\\", "#", " ", " ",
        "\n", "\r", "\t", "\u{3000}", "=", ";", ",", "[", "]", "$", "\u{301}", "😀",
    ];
    let mut rng = Lcg(0x70c3_11e5);
    for _ in 0..20_000 {
        let len = rng.next() % 24;
        let src: String = (0..len).map(|_| rng.pick(&alphabet)).collect();
        check(&src);
    }
}

#[test]
fn random_token_soup() {
    let pieces = [
        "key", "größe", "日本", "_x9", "a٣", "0", "42", "3.14", "1.", "1..2", "007", "1.5.6",
        "\"\"", "\"plain\"", "\"é\\\"q\"", "\"\\\\\"", "\"\\é\"", "\"two\nlines\"", "\"tab\\t😀\"",
        "=", ";", ",", "[", "]", "# note ü\n", " ", "  ", "\n", "\r\n", "\t", "\u{3000}",
    ];
    let mut rng = Lcg(0xdec0_de);
    for _ in 0..20_000 {
        let len = rng.next() % 16;
        let src: String = (0..len).map(|_| rng.pick(&pieces)).collect();
        check(&src);
    }
}
