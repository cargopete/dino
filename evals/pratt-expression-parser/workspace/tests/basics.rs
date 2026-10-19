use fixture::{eval, EvalError};

#[test]
fn precedence() {
    assert_eq!(eval("1 + 2 * 3"), Ok(7));
    assert_eq!(eval("(1 + 2) * 3"), Ok(9));
    assert_eq!(eval("10 - 4 - 3"), Ok(3));
}

#[test]
fn division_truncates() {
    assert_eq!(eval("7 / 2"), Ok(3));
    assert_eq!(eval("7 % 3"), Ok(1));
}

#[test]
fn exponent() {
    assert_eq!(eval("2 ^ 10"), Ok(1024));
}

#[test]
fn errors() {
    assert_eq!(eval("1 +"), Err(EvalError::UnexpectedEnd));
    assert_eq!(eval("1 / 0"), Err(EvalError::DivisionByZero { offset: 2 }));
}
