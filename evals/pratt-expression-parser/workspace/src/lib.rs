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
    let _ = src;
    todo!()
}
