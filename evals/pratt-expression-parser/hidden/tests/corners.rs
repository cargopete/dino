use fixture::{eval, EvalError};
use EvalError::*;

#[test]
fn literals_and_whitespace() {
    assert_eq!(eval("0"), Ok(0));
    assert_eq!(eval("007"), Ok(7));
    assert_eq!(eval("000000000000000000000000000000042"), Ok(42));
    assert_eq!(eval(" \t\r\n 12 \n"), Ok(12));
    assert_eq!(eval("9223372036854775807"), Ok(i64::MAX));
    assert_eq!(eval("1\n+\t2\r*3"), Ok(7));
}

#[test]
fn associativity() {
    assert_eq!(eval("100 / 10 / 5"), Ok(2));
    assert_eq!(eval("100 % 7 % 3"), Ok(2));
    assert_eq!(eval("2 * 3 % 4"), Ok(2));
    assert_eq!(eval("1 - 2 + 3"), Ok(2));
    assert_eq!(eval("2^3^2"), Ok(512));
    assert_eq!(eval("(2^3)^2"), Ok(64));
    assert_eq!(eval("2^1^0"), Ok(2));
}

#[test]
fn unary_minus_and_exponent() {
    assert_eq!(eval("-2^2"), Ok(-4));
    assert_eq!(eval("(-2)^2"), Ok(4));
    assert_eq!(eval("-2^3"), Ok(-8));
    assert_eq!(eval("--2^2"), Ok(4));
    assert_eq!(eval("---3"), Ok(-3));
    assert_eq!(eval("- - 3"), Ok(3));
    assert_eq!(eval("2*-3"), Ok(-6));
    assert_eq!(eval("2--3"), Ok(5));
    assert_eq!(eval("-3*-3"), Ok(9));
    assert_eq!(eval("-2*3^2"), Ok(-18));
    assert_eq!(eval("2^-0"), Ok(1));
    assert_eq!(eval("3^-(0)"), Ok(1));
    assert_eq!(eval("2^-1^0*5"), Err(NegativeExponent { offset: 1 }));
    assert_eq!(eval("2^--3*5"), Ok(40));
    assert_eq!(eval("-(2)^2"), Ok(-4));
}

#[test]
fn division_and_remainder_follow_rust() {
    assert_eq!(eval("-7 / 2"), Ok(-3));
    assert_eq!(eval("7 / -2"), Ok(-3));
    assert_eq!(eval("-7 % 2"), Ok(-1));
    assert_eq!(eval("7 % -2"), Ok(1));
    assert_eq!(eval("-7 % -2"), Ok(-1));
    assert_eq!(eval("0 % 5"), Ok(0));
}

#[test]
fn powers() {
    assert_eq!(eval("0^0"), Ok(1));
    assert_eq!(eval("0^5"), Ok(0));
    assert_eq!(eval("5^0"), Ok(1));
    assert_eq!(eval("(-1)^9999999999"), Ok(-1));
    assert_eq!(eval("(-1)^10000000000"), Ok(1));
    assert_eq!(eval("1^9223372036854775807"), Ok(1));
    assert_eq!(eval("0^9223372036854775807"), Ok(0));
    assert_eq!(eval("(0-5)^0"), Ok(1));
    assert_eq!(eval("2^62"), Ok(1 << 62));
    assert_eq!(eval("(-2)^63"), Ok(i64::MIN));
    assert_eq!(eval("2^63"), Err(Overflow { offset: 1 }));
    assert_eq!(eval("2^4294967296"), Err(Overflow { offset: 1 }));
    assert_eq!(eval("2^4294967297"), Err(Overflow { offset: 1 }));
    assert_eq!(eval("(-2)^4294967296"), Err(Overflow { offset: 4 }));
    assert_eq!(eval("3^40"), Err(Overflow { offset: 1 }));
    assert_eq!(eval("3^39"), Ok(4052555153018976267));
    assert_eq!(eval("2 ^ -1"), Err(NegativeExponent { offset: 2 }));
    assert_eq!(eval("0 ^ -1"), Err(NegativeExponent { offset: 2 }));
    assert_eq!(eval("1 ^ -9223372036854775807"), Err(NegativeExponent { offset: 2 }));
}

#[test]
fn overflow_offsets() {
    assert_eq!(eval("9223372036854775808"), Err(Overflow { offset: 0 }));
    assert_eq!(eval("-9223372036854775808"), Err(Overflow { offset: 1 }));
    assert_eq!(eval("  99999999999999999999999"), Err(Overflow { offset: 2 }));
    assert_eq!(eval("-9223372036854775807 - 1"), Ok(i64::MIN));
    assert_eq!(eval("-9223372036854775807 - 2"), Err(Overflow { offset: 21 }));
    assert_eq!(eval("9223372036854775807 + 1"), Err(Overflow { offset: 20 }));
    assert_eq!(eval("4611686018427387904 * 2"), Err(Overflow { offset: 20 }));
    assert_eq!(eval("-(-9223372036854775807 - 1)"), Err(Overflow { offset: 0 }));
    assert_eq!(eval("(-9223372036854775807 - 1) * -1"), Err(Overflow { offset: 27 }));
    assert_eq!(eval("(-9223372036854775807 - 1) / -1"), Err(Overflow { offset: 27 }));
    assert_eq!(eval("(-9223372036854775807 - 1) % -1"), Err(Overflow { offset: 27 }));
    assert_eq!(eval("(-9223372036854775807 - 1) / 1"), Ok(i64::MIN));
    // Intermediate overflow is an error even when the end result would fit.
    assert_eq!(eval("9223372036854775807 + 1 - 1"), Err(Overflow { offset: 20 }));
}

#[test]
fn evaluation_order() {
    assert_eq!(eval("1/0 + 2^-1"), Err(DivisionByZero { offset: 1 }));
    assert_eq!(eval("2^-1 + 1/0"), Err(NegativeExponent { offset: 1 }));
    assert_eq!(eval("(1/0) * 99999999999999999999"), Err(DivisionByZero { offset: 2 }));
    assert_eq!(eval("99999999999999999999 * (1/0)"), Err(Overflow { offset: 0 }));
    assert_eq!(eval("5 % (3 - 3)"), Err(DivisionByZero { offset: 2 }));
    assert_eq!(eval("0 / 0"), Err(DivisionByZero { offset: 2 }));
    assert_eq!(eval("2 ^ (1/0)"), Err(DivisionByZero { offset: 6 }));
    assert_eq!(eval("(2^64) ^ 0"), Err(Overflow { offset: 2 }));
    assert_eq!(eval("-(1/0)"), Err(DivisionByZero { offset: 3 }));
}

#[test]
fn unexpected_end() {
    assert_eq!(eval(""), Err(UnexpectedEnd));
    assert_eq!(eval("   \n\t"), Err(UnexpectedEnd));
    assert_eq!(eval("-"), Err(UnexpectedEnd));
    assert_eq!(eval("2^"), Err(UnexpectedEnd));
    assert_eq!(eval("(1"), Err(UnexpectedEnd));
    assert_eq!(eval("((1)"), Err(UnexpectedEnd));
    assert_eq!(eval("1 * (2 + "), Err(UnexpectedEnd));
    assert_eq!(eval("2^-"), Err(UnexpectedEnd));
}

#[test]
fn unexpected_token() {
    assert_eq!(eval("+1"), Err(UnexpectedToken { offset: 0 }));
    assert_eq!(eval("1 2"), Err(UnexpectedToken { offset: 2 }));
    assert_eq!(eval("1)"), Err(UnexpectedToken { offset: 1 }));
    assert_eq!(eval("()"), Err(UnexpectedToken { offset: 1 }));
    assert_eq!(eval("(1))"), Err(UnexpectedToken { offset: 3 }));
    assert_eq!(eval("2^^3"), Err(UnexpectedToken { offset: 2 }));
    assert_eq!(eval("1 + * 2"), Err(UnexpectedToken { offset: 4 }));
    assert_eq!(eval("1 (2)"), Err(UnexpectedToken { offset: 2 }));
    assert_eq!(eval("(2)(3)"), Err(UnexpectedToken { offset: 3 }));
    assert_eq!(eval("2 ^ + 3"), Err(UnexpectedToken { offset: 4 }));
    assert_eq!(eval("1 + x"), Err(UnexpectedToken { offset: 4 }));
    assert_eq!(eval("1.5"), Err(UnexpectedToken { offset: 1 }));
    assert_eq!(eval("1 + é"), Err(UnexpectedToken { offset: 4 }));
    assert_eq!(eval("1 +\u{a0}2"), Err(UnexpectedToken { offset: 3 }));
    assert_eq!(eval("1 +\u{b}2"), Err(UnexpectedToken { offset: 3 }));
    assert_eq!(eval("1 +\u{c}2"), Err(UnexpectedToken { offset: 3 }));
    assert_eq!(eval("-+1"), Err(UnexpectedToken { offset: 1 }));
    assert_eq!(eval("2 ** 3"), Err(UnexpectedToken { offset: 3 }));
}

#[test]
fn leftmost_syntax_error_wins() {
    assert_eq!(eval("1 + ) @"), Err(UnexpectedToken { offset: 4 }));
    assert_eq!(eval("1 @ )"), Err(UnexpectedToken { offset: 2 }));
    assert_eq!(eval("(1 + 2 @"), Err(UnexpectedToken { offset: 7 }));
    assert_eq!(eval("1 2 é"), Err(UnexpectedToken { offset: 2 }));
}

#[test]
fn syntax_errors_beat_evaluation_errors() {
    assert_eq!(eval("1/0 )"), Err(UnexpectedToken { offset: 4 }));
    assert_eq!(eval("1/0 +"), Err(UnexpectedEnd));
    assert_eq!(eval("99999999999999999999 1"), Err(UnexpectedToken { offset: 21 }));
    assert_eq!(eval("2^-1 x"), Err(UnexpectedToken { offset: 5 }));
    assert_eq!(eval("(9223372036854775807 + 1"), Err(UnexpectedEnd));
}

#[test]
fn deep_nesting() {
    let depth = 256;
    let src = format!("{}7{}", "(".repeat(depth), ")".repeat(depth));
    assert_eq!(eval(&src), Ok(7));
    let src = format!("{}7", "-".repeat(depth + 1));
    assert_eq!(eval(&src), Ok(-7));
    // Right-associative, so the tower overflows at its fifth `^` from the right.
    let src = format!("{}1", "2^".repeat(depth));
    assert_eq!(eval(&src), Err(Overflow { offset: 2 * (depth - 4) - 1 }));
    let src = format!("{}1", "1^".repeat(depth));
    assert_eq!(eval(&src), Ok(1));
    let src = format!("{}7{}", "(".repeat(depth), ")".repeat(depth - 1));
    assert_eq!(eval(&src), Err(UnexpectedEnd));
}
