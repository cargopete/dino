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
    let mut parser = Parser { src, pos: 0 };
    parser.document().map_err(|(pos, kind)| ParseError {
        line: 1 + src[..pos].matches('\n').count(),
        kind,
    })
}

type Fail = (usize, ErrorKind);

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

fn is_bare(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_control(c: char) -> bool {
    (c < ' ' && c != '\t') || c == '\u{7f}'
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(offset)
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn fail<T>(&self, kind: ErrorKind) -> Result<T, Fail> {
        Err((self.pos, kind))
    }

    fn expect(&mut self, c: char) -> Result<(), Fail> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            self.fail(ErrorKind::Syntax)
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    /// Consumes a newline if one is next.
    fn newline(&mut self) -> bool {
        if self.rest().starts_with('\n') {
            self.pos += 1;
            true
        } else if self.rest().starts_with("\r\n") {
            self.pos += 2;
            true
        } else {
            false
        }
    }

    fn comment(&mut self) -> Result<(), Fail> {
        if self.peek() != Some('#') {
            return Ok(());
        }
        self.pos += 1;
        while let Some(c) = self.peek() {
            if c == '\n' || self.rest().starts_with("\r\n") {
                break;
            }
            if is_control(c) {
                return self.fail(ErrorKind::Syntax);
            }
            self.bump();
        }
        Ok(())
    }

    fn line_end(&mut self) -> Result<(), Fail> {
        self.skip_ws();
        self.comment()?;
        if self.peek().is_none() || self.newline() {
            Ok(())
        } else {
            self.fail(ErrorKind::Syntax)
        }
    }

    fn document(&mut self) -> Result<Table, Fail> {
        let mut root = Table::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => break,
                Some('#' | '\n' | '\r') => {}
                Some(_) => self.key_value(&mut root)?,
            }
            self.line_end()?;
        }
        Ok(root)
    }

    /// Parses `key = value` into `table`. The key is checked before the value
    /// is read, because a redefinition is reported at the key.
    fn key_value(&mut self, table: &mut Table) -> Result<(), Fail> {
        let start = self.pos;
        let key = self.simple_key()?;
        if table.contains_key(&key) {
            return Err((start, ErrorKind::Redefinition));
        }
        self.skip_ws();
        self.expect('=')?;
        self.skip_ws();
        let value = self.value()?;
        table.insert(key, value);
        Ok(())
    }

    fn simple_key(&mut self) -> Result<String, Fail> {
        match self.peek() {
            Some('"') => self.basic_string(),
            Some('\'') => self.literal_string(),
            Some(c) if is_bare(c) => {
                let start = self.pos;
                while self.peek().is_some_and(is_bare) {
                    self.pos += 1;
                }
                Ok(self.src[start..self.pos].to_owned())
            }
            _ => self.fail(ErrorKind::Syntax),
        }
    }

    fn value(&mut self) -> Result<Value, Fail> {
        match self.peek() {
            Some('"') if self.rest().starts_with("\"\"\"") => {
                self.multiline_basic().map(Value::String)
            }
            Some('"') => self.basic_string().map(Value::String),
            Some('\'') if self.rest().starts_with("'''") => {
                self.multiline_literal().map(Value::String)
            }
            Some('\'') => self.literal_string().map(Value::String),
            Some('t') if self.rest().starts_with("true") => {
                self.pos += 4;
                Ok(Value::Boolean(true))
            }
            Some('f') if self.rest().starts_with("false") => {
                self.pos += 5;
                Ok(Value::Boolean(false))
            }
            Some('+' | '-' | '0'..='9') => self.integer().map(Value::Integer),
            _ => self.fail(ErrorKind::Syntax),
        }
    }

    fn integer(&mut self) -> Result<i64, Fail> {
        let start = self.pos;
        let radix = match (self.peek(), self.peek_at(1)) {
            (Some('0'), Some('x')) => 16,
            (Some('0'), Some('o')) => 8,
            (Some('0'), Some('b')) => 2,
            _ => 10,
        };
        let mut negative = false;
        if radix == 10 {
            match self.peek() {
                Some('+') => self.pos += 1,
                Some('-') => {
                    negative = true;
                    self.pos += 1;
                }
                _ => {}
            }
        } else {
            self.pos += 2;
        }
        let digit = |c: Option<char>| c.and_then(|c| c.to_digit(radix));
        let Some(first) = digit(self.peek()) else {
            return self.fail(ErrorKind::Syntax);
        };
        self.pos += 1;
        let mut magnitude = i128::from(first);
        let mut overflow = false;
        if !(radix == 10 && first == 0) {
            loop {
                match self.peek() {
                    Some('_') if digit(self.peek_at(1)).is_some() => self.pos += 1,
                    Some('_') => return self.fail(ErrorKind::Syntax),
                    c => match digit(c) {
                        Some(d) => {
                            self.pos += 1;
                            match magnitude
                                .checked_mul(i128::from(radix))
                                .and_then(|m| m.checked_add(i128::from(d)))
                            {
                                Some(m) => magnitude = m,
                                None => overflow = true,
                            }
                        }
                        None => break,
                    },
                }
            }
        }
        let value = if negative { -magnitude } else { magnitude };
        match i64::try_from(value) {
            Ok(value) if !overflow => Ok(value),
            _ => Err((start, ErrorKind::Integer)),
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), Fail> {
        let start = self.pos;
        self.pos += 1;
        let bad = Err((start, ErrorKind::Escape));
        let c = match self.bump() {
            Some('b') => '\u{8}',
            Some('t') => '\t',
            Some('n') => '\n',
            Some('f') => '\u{c}',
            Some('r') => '\r',
            Some('"') => '"',
            Some('\\') => '\\',
            Some(u @ ('u' | 'U')) => {
                let width = if u == 'u' { 4 } else { 8 };
                let hex = self.rest().get(..width).unwrap_or("");
                if hex.len() != width || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return bad;
                }
                self.pos += width;
                match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => c,
                    None => return bad,
                }
            }
            _ => return bad,
        };
        out.push(c);
        Ok(())
    }

    fn basic_string(&mut self) -> Result<String, Fail> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                Some('"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some('\\') => self.escape(&mut out)?,
                Some(c) if !is_control(c) => {
                    out.push(c);
                    self.bump();
                }
                _ => return self.fail(ErrorKind::Syntax),
            }
        }
    }

    fn literal_string(&mut self) -> Result<String, Fail> {
        self.pos += 1;
        let start = self.pos;
        loop {
            match self.peek() {
                Some('\'') => {
                    let out = self.src[start..self.pos].to_owned();
                    self.pos += 1;
                    return Ok(out);
                }
                Some(c) if !is_control(c) => {
                    self.bump();
                }
                _ => return self.fail(ErrorKind::Syntax),
            }
        }
    }

    /// Closes a multi-line string at a run of `quote`, keeping up to two of
    /// the run as content.
    fn close_multiline(&mut self, quote: char, out: &mut String) -> bool {
        let run = self.rest().chars().take_while(|&c| c == quote).count();
        if run < 3 {
            return false;
        }
        let taken = run.min(5);
        for _ in 3..taken {
            out.push(quote);
        }
        self.pos += taken;
        true
    }

    fn multiline_basic(&mut self) -> Result<String, Fail> {
        self.pos += 3;
        self.newline();
        let mut out = String::new();
        loop {
            if self.close_multiline('"', &mut out) {
                return Ok(out);
            }
            if self.newline() {
                out.push('\n');
                continue;
            }
            match self.peek() {
                Some('\\') => {
                    let start = self.pos;
                    self.pos += 1;
                    self.skip_ws();
                    if self.newline() {
                        loop {
                            self.skip_ws();
                            if !self.newline() {
                                break;
                            }
                        }
                    } else {
                        self.pos = start;
                        self.escape(&mut out)?;
                    }
                }
                Some(c) if !is_control(c) => {
                    out.push(c);
                    self.bump();
                }
                _ => return self.fail(ErrorKind::Syntax),
            }
        }
    }

    fn multiline_literal(&mut self) -> Result<String, Fail> {
        self.pos += 3;
        self.newline();
        let mut out = String::new();
        loop {
            if self.close_multiline('\'', &mut out) {
                return Ok(out);
            }
            if self.newline() {
                out.push('\n');
                continue;
            }
            match self.peek() {
                Some(c) if !is_control(c) => {
                    out.push(c);
                    self.bump();
                }
                _ => return self.fail(ErrorKind::Syntax),
            }
        }
    }
}
