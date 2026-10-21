mod common;
use common::*;
use fixture::ErrorKind::*;

#[test]
fn decimal_integers() {
    assert_eq!(value("0"), i(0));
    assert_eq!(value("+0"), i(0));
    assert_eq!(value("-0"), i(0));
    assert_eq!(value("+99"), i(99));
    assert_eq!(value("-17"), i(-17));
    assert_eq!(value("1_000"), i(1000));
    assert_eq!(value("5_349_221"), i(5_349_221));
    assert_eq!(value("1_2_3_4_5"), i(12345));
    assert_eq!(value("9223372036854775807"), i(i64::MAX));
    assert_eq!(value("-9223372036854775808"), i(i64::MIN));
}

#[test]
fn other_bases() {
    assert_eq!(value("0xDEADBEEF"), i(0xDEAD_BEEF));
    assert_eq!(value("0xdead_beef"), i(0xDEAD_BEEF));
    assert_eq!(value("0x00ff"), i(255));
    assert_eq!(value("0o755"), i(0o755));
    assert_eq!(value("0o0_7"), i(7));
    assert_eq!(value("0b1101_0110"), i(0b1101_0110));
    assert_eq!(value("0x7fffffffffffffff"), i(i64::MAX));
}

#[test]
fn booleans() {
    assert_eq!(value("true"), fixture::Value::Boolean(true));
    assert_eq!(value("false"), fixture::Value::Boolean(false));
}

#[test]
fn malformed_integers_are_syntax_errors() {
    for bad in [
        "007", "00", "-01", "1__2", "_1", "1_", "0x", "0x_1", "0xg", "+0x10", "-0b1",
        "0X10", "0b2", "0o8", "1.5", "1e3", "++1", "+", "-", "1 2",
    ] {
        fails(&format!("v = {bad}\n"), 1, Syntax);
    }
}

#[test]
fn out_of_range_integers() {
    fails("v = 9223372036854775808\n", 1, Integer);
    fails("v = -9223372036854775809\n", 1, Integer);
    fails("v = 0x8000000000000000\n", 1, Integer);
    fails("v = 0b1_0000000000000000000000000000000000000000000000000000000000000000\n", 1, Integer);
    fails("v = 123456789012345678901234567890123456789012345678901234567890\n", 1, Integer);
    fails("a = 1\n\nv = 99999999999999999999 # far too big\n", 3, Integer);
}

#[test]
fn malformed_booleans_and_bare_words() {
    for bad in ["True", "FALSE", "tru", "truex", "yes", "nan", "inf", "1979-05-27"] {
        fails(&format!("v = {bad}\n"), 1, Syntax);
    }
}

#[test]
fn a_missing_value() {
    fails("v =\n", 1, Syntax);
    fails("v = # nothing\n", 1, Syntax);
    fails("v = ", 1, Syntax);
}
