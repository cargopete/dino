// Generates expression trees, prints them with only the parentheses the
// grammar needs (plus some redundant ones), and compares `eval` against a
// tree-walking oracle that computes in i128.

use fixture::{eval, EvalError};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

enum Tree {
    Num(String),
    Neg(Box<Tree>),
    Bin(u8, Box<Tree>, Box<Tree>),
}

// What the grammar accepts in a position, from loosest to tightest.
#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Slot {
    Expr,
    Term,
    Unary,
    Atom,
}

fn literal(rng: &mut Lcg) -> String {
    let digits = match rng.below(20) {
        0..=11 => rng.below(10).to_string(),
        12..=15 => rng.below(1000).to_string(),
        16 => (i64::MAX as u64 - rng.below(3)).to_string(),
        17 => (i64::MAX as u64 + 1 + rng.below(2)).to_string(),
        18 => (1u64 << (31 + rng.below(3))).to_string(),
        _ => "12345678901234567890123".to_string(),
    };
    if rng.below(10) == 0 {
        format!("00{digits}")
    } else {
        digits
    }
}

fn gen(rng: &mut Lcg, depth: u32) -> Tree {
    if depth == 0 || rng.below(4) == 0 {
        return Tree::Num(literal(rng));
    }
    match rng.below(9) {
        0 => Tree::Neg(Box::new(gen(rng, depth - 1))),
        1 => {
            // Small exponents most of the time, so powers are not all overflow.
            let exp = if rng.below(2) == 0 {
                Tree::Num(rng.below(5).to_string())
            } else {
                gen(rng, depth - 1)
            };
            Tree::Bin(b'^', Box::new(gen(rng, depth - 1)), Box::new(exp))
        }
        n => {
            let op = b"+-*/%+-"[n as usize - 2];
            Tree::Bin(op, Box::new(gen(rng, depth - 1)), Box::new(gen(rng, depth - 1)))
        }
    }
}

struct Printer<'r> {
    rng: &'r mut Lcg,
    out: String,
}

impl Printer<'_> {
    fn space(&mut self) {
        let ws = [" ", "", "", "\t", "\n", "  ", "\r\n"];
        let pick = ws[self.rng.below(ws.len() as u64) as usize];
        self.out.push_str(pick);
    }

    fn tok(&mut self, s: &str) -> usize {
        self.space();
        let at = self.out.len();
        self.out.push_str(s);
        at
    }

    // Prints `tree` so that it parses back as the same tree in `slot`, and
    // returns a mirror tree annotated with each node's byte offset.
    fn print(&mut self, tree: &Tree, slot: Slot) -> Annotated {
        let own = match tree {
            Tree::Num(_) => Slot::Atom,
            Tree::Neg(_) => Slot::Unary,
            Tree::Bin(b'^', _, _) => Slot::Unary,
            Tree::Bin(b'+' | b'-', _, _) => Slot::Expr,
            Tree::Bin(..) => Slot::Term,
        };
        if own < slot || self.rng.below(12) == 0 {
            self.tok("(");
            let inner = self.print_bare(tree);
            self.tok(")");
            return inner;
        }
        self.print_bare(tree)
    }

    fn print_bare(&mut self, tree: &Tree) -> Annotated {
        match tree {
            Tree::Num(digits) => {
                let at = self.tok(digits);
                Annotated::Num(digits.clone(), at)
            }
            Tree::Neg(inner) => {
                let at = self.tok("-");
                let inner = self.print(inner, Slot::Unary);
                Annotated::Neg(Box::new(inner), at)
            }
            Tree::Bin(op, lhs, rhs) => {
                let (left_slot, right_slot) = match op {
                    b'^' => (Slot::Atom, Slot::Unary),
                    b'+' | b'-' => (Slot::Expr, Slot::Term),
                    _ => (Slot::Term, Slot::Unary),
                };
                let lhs = self.print(lhs, left_slot);
                let at = self.tok(std::str::from_utf8(&[*op]).unwrap());
                let rhs = self.print(rhs, right_slot);
                Annotated::Bin(*op, Box::new(lhs), Box::new(rhs), at)
            }
        }
    }
}

enum Annotated {
    Num(String, usize),
    Neg(Box<Annotated>, usize),
    Bin(u8, Box<Annotated>, Box<Annotated>, usize),
}

fn fits(v: i128) -> bool {
    v >= i64::MIN as i128 && v <= i64::MAX as i128
}

fn oracle(tree: &Annotated) -> Result<i128, EvalError> {
    match tree {
        Annotated::Num(digits, at) => {
            let mut v: i128 = 0;
            for d in digits.bytes() {
                v = v * 10 + (d - b'0') as i128;
                if v > i64::MAX as i128 {
                    return Err(EvalError::Overflow { offset: *at });
                }
            }
            Ok(v)
        }
        Annotated::Neg(inner, at) => {
            let v = -oracle(inner)?;
            if fits(v) {
                Ok(v)
            } else {
                Err(EvalError::Overflow { offset: *at })
            }
        }
        Annotated::Bin(op, lhs, rhs, at) => {
            let a = oracle(lhs)?;
            let b = oracle(rhs)?;
            let offset = *at;
            let v = match op {
                b'+' => a + b,
                b'-' => a - b,
                b'*' => a * b,
                b'/' | b'%' if b == 0 => return Err(EvalError::DivisionByZero { offset }),
                b'/' | b'%' if a == i64::MIN as i128 && b == -1 => {
                    return Err(EvalError::Overflow { offset })
                }
                b'/' => a / b,
                b'%' => a % b,
                _ if b < 0 => return Err(EvalError::NegativeExponent { offset }),
                _ if b == 0 => 1,
                _ if a == 0 || a == 1 => a,
                _ if a == -1 => {
                    if b % 2 == 0 {
                        1
                    } else {
                        -1
                    }
                }
                _ => {
                    let mut acc: i128 = 1;
                    for _ in 0..b.min(64) {
                        acc *= a;
                        if !fits(acc) {
                            return Err(EvalError::Overflow { offset });
                        }
                    }
                    if b > 64 {
                        return Err(EvalError::Overflow { offset });
                    }
                    acc
                }
            };
            if fits(v) {
                Ok(v)
            } else {
                Err(EvalError::Overflow { offset })
            }
        }
    }
}

#[test]
fn matches_oracle_on_generated_expressions() {
    let mut rng = Lcg(0x5eed_0f_9a77);
    let mut ok = 0;
    for case in 0..20_000 {
        let tree = gen(&mut rng, 1 + (case % 7) as u32);
        let mut printer = Printer { rng: &mut rng, out: String::new() };
        let annotated = printer.print(&tree, Slot::Expr);
        printer.space();
        let src = printer.out;
        let expected = oracle(&annotated).map(|v| v as i64);
        ok += expected.is_ok() as u32;
        assert_eq!(eval(&src), expected, "source: {src:?}");
    }
    assert!(ok > 5_000, "generator is producing too many errors: {ok} ok");
}
