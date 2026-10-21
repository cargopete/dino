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
    let mut parser = Parser {
        src,
        pos: 0,
        ids: 0,
    };
    parser.document().map_err(|(pos, kind)| ParseError {
        line: 1 + src[..pos].matches('\n').count(),
        kind,
    })
}

type Fail = (usize, ErrorKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Implicit,
    Header,
    Dotted(usize),
}

#[derive(Debug)]
struct Node {
    kind: Kind,
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug)]
enum Entry {
    Value(Value),
    Table(Node),
    Tables(Vec<Node>),
}

impl Node {
    fn new(kind: Kind) -> Self {
        Node {
            kind,
            entries: BTreeMap::new(),
        }
    }

    fn into_table(self) -> Table {
        self.entries
            .into_iter()
            .map(|(key, entry)| {
                let value = match entry {
                    Entry::Value(value) => value,
                    Entry::Table(node) => Value::Table(node.into_table()),
                    Entry::Tables(nodes) => Value::Array(
                        nodes
                            .into_iter()
                            .map(|node| Value::Table(node.into_table()))
                            .collect(),
                    ),
                };
                (key, value)
            })
            .collect()
    }

    /// Follows a header path, the last array-of-tables element at each step.
    fn walk(&mut self, path: &[String]) -> &mut Node {
        let mut node = self;
        for key in path {
            node = match node.entries.get_mut(key) {
                Some(Entry::Table(next)) => next,
                Some(Entry::Tables(nodes)) => nodes.last_mut().unwrap(),
                _ => unreachable!("a header path always names a table"),
            };
        }
        node
    }

    fn insert_dotted(&mut self, keys: &[String], value: Value, section: usize) -> Result<(), ()> {
        let (last, path) = keys.split_last().unwrap();
        let mut node = self;
        for key in path {
            node = match node
                .entries
                .entry(key.clone())
                .or_insert_with(|| Entry::Table(Node::new(Kind::Dotted(section))))
            {
                Entry::Table(next) if next.kind == Kind::Dotted(section) => next,
                _ => return Err(()),
            };
        }
        if node.entries.contains_key(last) {
            return Err(());
        }
        node.entries.insert(last.clone(), Entry::Value(value));
        Ok(())
    }

    /// The value a dotted key has just been given.
    fn slot(&mut self, keys: &[String]) -> &mut Value {
        let (last, path) = keys.split_last().unwrap();
        let mut node = self;
        for key in path {
            node = match node.entries.get_mut(key) {
                Some(Entry::Table(next)) => next,
                _ => unreachable!("inserted just now"),
            };
        }
        match node.entries.get_mut(last) {
            Some(Entry::Value(value)) => value,
            _ => unreachable!("inserted just now"),
        }
    }

    fn define(&mut self, keys: &[String], array: bool) -> Result<(), ()> {
        let (last, path) = keys.split_last().unwrap();
        let mut node = self;
        for key in path {
            node = match node
                .entries
                .entry(key.clone())
                .or_insert_with(|| Entry::Table(Node::new(Kind::Implicit)))
            {
                Entry::Table(next) => next,
                Entry::Tables(nodes) => nodes.last_mut().unwrap(),
                Entry::Value(_) => return Err(()),
            };
        }
        match (node.entries.get_mut(last), array) {
            (None, false) => {
                node.entries
                    .insert(last.clone(), Entry::Table(Node::new(Kind::Header)));
            }
            (None, true) => {
                node.entries
                    .insert(last.clone(), Entry::Tables(vec![Node::new(Kind::Header)]));
            }
            (Some(Entry::Table(existing)), false) if existing.kind == Kind::Implicit => {
                existing.kind = Kind::Header;
            }
            (Some(Entry::Tables(nodes)), true) => nodes.push(Node::new(Kind::Header)),
            _ => return Err(()),
        }
        Ok(())
    }
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    /// Numbers each section, so dotted keys can tell whose table they are in.
    ids: usize,
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

    fn fresh(&mut self) -> usize {
        self.ids += 1;
        self.ids
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
        let mut root = Node::new(Kind::Header);
        let mut current: Vec<String> = Vec::new();
        let mut section = 0;
        loop {
            self.skip_ws();
            match self.peek() {
                None => break,
                Some('#') => {}
                Some('\n' | '\r') => {}
                Some('[') => {
                    let start = self.pos;
                    self.pos += 1;
                    let array = self.peek() == Some('[');
                    if array {
                        self.pos += 1;
                    }
                    self.skip_ws();
                    let keys = self.key()?;
                    if root.define(&keys, array).is_err() {
                        return Err((start, ErrorKind::Redefinition));
                    }
                    self.skip_ws();
                    self.expect(']')?;
                    if array {
                        self.expect(']')?;
                    }
                    section = self.fresh();
                    current = keys;
                }
                Some(_) => {
                    self.key_value(root.walk(&current), section)?;
                }
            }
            self.line_end()?;
        }
        Ok(root.into_table())
    }

    /// Parses `key = value` into `node`. The key is checked before the value
    /// is read, because a redefinition is reported at the key.
    fn key_value(&mut self, node: &mut Node, section: usize) -> Result<(), Fail> {
        let start = self.pos;
        let keys = self.key()?;
        if node
            .insert_dotted(&keys, Value::Boolean(false), section)
            .is_err()
        {
            return Err((start, ErrorKind::Redefinition));
        }
        self.skip_ws();
        self.expect('=')?;
        self.skip_ws();
        *node.slot(&keys) = self.value()?;
        Ok(())
    }

    fn key(&mut self) -> Result<Vec<String>, Fail> {
        let mut keys = vec![self.simple_key()?];
        loop {
            let save = self.pos;
            self.skip_ws();
            if self.peek() != Some('.') {
                self.pos = save;
                return Ok(keys);
            }
            self.pos += 1;
            self.skip_ws();
            keys.push(self.simple_key()?);
        }
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
