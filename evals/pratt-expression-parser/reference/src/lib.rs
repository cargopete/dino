/// Why `eval` failed. Every offset is a byte offset into the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalError {
    UnexpectedToken { offset: usize },
    UnexpectedEnd,
    DivisionByZero { offset: usize },
    Overflow { offset: usize },
    NegativeExponent { offset: usize },
}

pub fn eval(src: &str) -> Result<i64, EvalError> {
    let mut parser = Parser { tokens: lex(src), pos: 0 };
    let node = parser.expr()?;
    match parser.peek() {
        Tok::End => evaluate(&node),
        _ => Err(parser.unexpected()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    Num(&'a str),
    Op(u8),
    Open,
    Close,
    Invalid,
    End,
}

fn lex(src: &str) -> Vec<(Tok<'_>, usize)> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let start = i;
        let tok = match b {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'0'..=b'9' => {
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                out.push((Tok::Num(&src[start..i]), start));
                continue;
            }
            b'+' | b'-' | b'*' | b'/' | b'%' | b'^' => Tok::Op(b),
            b'(' => Tok::Open,
            b')' => Tok::Close,
            // Nothing after an invalid character can matter.
            _ => {
                out.push((Tok::Invalid, start));
                return out;
            }
        };
        out.push((tok, start));
        i += 1;
    }
    out.push((Tok::End, src.len()));
    out
}

enum Node<'a> {
    Num(&'a str, usize),
    Neg(Box<Node<'a>>, usize),
    Bin(u8, Box<Node<'a>>, Box<Node<'a>>, usize),
}

struct Parser<'a> {
    tokens: Vec<(Tok<'a>, usize)>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Tok<'a> {
        self.tokens[self.pos].0
    }

    fn offset(&self) -> usize {
        self.tokens[self.pos].1
    }

    fn unexpected(&self) -> EvalError {
        match self.peek() {
            Tok::End => EvalError::UnexpectedEnd,
            _ => EvalError::UnexpectedToken { offset: self.offset() },
        }
    }

    fn expr(&mut self) -> Result<Node<'a>, EvalError> {
        let mut lhs = self.term()?;
        while let Tok::Op(op @ (b'+' | b'-')) = self.peek() {
            let at = self.offset();
            self.pos += 1;
            let rhs = self.term()?;
            lhs = Node::Bin(op, Box::new(lhs), Box::new(rhs), at);
        }
        Ok(lhs)
    }

    fn term(&mut self) -> Result<Node<'a>, EvalError> {
        let mut lhs = self.unary()?;
        while let Tok::Op(op @ (b'*' | b'/' | b'%')) = self.peek() {
            let at = self.offset();
            self.pos += 1;
            let rhs = self.unary()?;
            lhs = Node::Bin(op, Box::new(lhs), Box::new(rhs), at);
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Node<'a>, EvalError> {
        if self.peek() == Tok::Op(b'-') {
            let at = self.offset();
            self.pos += 1;
            let inner = self.unary()?;
            return Ok(Node::Neg(Box::new(inner), at));
        }
        let base = self.atom()?;
        if self.peek() == Tok::Op(b'^') {
            let at = self.offset();
            self.pos += 1;
            let exp = self.unary()?;
            return Ok(Node::Bin(b'^', Box::new(base), Box::new(exp), at));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Node<'a>, EvalError> {
        match self.peek() {
            Tok::Num(text) => {
                let at = self.offset();
                self.pos += 1;
                Ok(Node::Num(text, at))
            }
            Tok::Open => {
                self.pos += 1;
                let inner = self.expr()?;
                if self.peek() != Tok::Close {
                    return Err(self.unexpected());
                }
                self.pos += 1;
                Ok(inner)
            }
            _ => Err(self.unexpected()),
        }
    }
}

fn evaluate(node: &Node) -> Result<i64, EvalError> {
    match node {
        Node::Num(text, at) => text.parse().map_err(|_| EvalError::Overflow { offset: *at }),
        Node::Neg(inner, at) => {
            let v = evaluate(inner)?;
            v.checked_neg().ok_or(EvalError::Overflow { offset: *at })
        }
        Node::Bin(op, lhs, rhs, at) => {
            let a = evaluate(lhs)?;
            let b = evaluate(rhs)?;
            let offset = *at;
            let result = match op {
                b'+' => a.checked_add(b),
                b'-' => a.checked_sub(b),
                b'*' => a.checked_mul(b),
                b'/' | b'%' if b == 0 => return Err(EvalError::DivisionByZero { offset }),
                b'/' => a.checked_div(b),
                b'%' => a.checked_rem(b),
                _ if b < 0 => return Err(EvalError::NegativeExponent { offset }),
                _ => pow(a, b),
            };
            result.ok_or(EvalError::Overflow { offset })
        }
    }
}

fn pow(base: i64, exp: i64) -> Option<i64> {
    match base {
        _ if exp == 0 => Some(1),
        0 | 1 => Some(base),
        -1 => Some(if exp % 2 == 0 { 1 } else { -1 }),
        // |base| >= 2, so any exponent past 63 leaves i64.
        _ if exp > 63 => None,
        _ => base.checked_pow(exp as u32),
    }
}
