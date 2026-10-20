mod common;
use common::*;
use fixture::ErrorKind::*;

#[test]
fn basic_strings_and_escapes() {
    assert_eq!(value(r#""""#), s(""));
    assert_eq!(value(r#""a b""#), s("a b"));
    assert_eq!(
        value(r#""\b\t\n\f\r\"\\""#),
        s("\u{8}\t\n\u{c}\r\"\\")
    );
    assert_eq!(value(r#""\u00e9\u0041""#), s("éA"));
    assert_eq!(value(r#""\U0001F600!""#), s("😀!"));
    assert_eq!(value(r#""tab	inside""#), s("tab\tinside"));
    assert_eq!(value(r##""# not a comment""##), s("# not a comment"));
    assert_eq!(value("\"ünïcödé ✓\""), s("ünïcödé ✓"));
}

#[test]
fn bad_escapes() {
    fails("v = \"\\q\"\n", 1, Escape);
    fails("v = \"\\x41\"\n", 1, Escape);
    fails("v = \"\\u12\"\n", 1, Escape);
    fails("v = \"\\u12G4\"\n", 1, Escape);
    fails("v = \"\\U0000004\"\n", 1, Escape);
    fails("v = \"\\uD800\"\n", 1, Escape);
    fails("v = \"\\U00110000\"\n", 1, Escape);
    fails("v = \"\\ \"\n", 1, Escape);
    fails("v = \"ok\"\nw = \"fine \\a\"\n", 2, Escape);
    fails("v = \"\\q", 1, Escape);
}

#[test]
fn unterminated_and_control_characters() {
    fails("v = \"abc\n\"\n", 1, Syntax);
    fails("v = \"abc", 1, Syntax);
    fails("v = 'abc\n", 1, Syntax);
    fails("v = \"a\u{1}b\"\n", 1, Syntax);
    fails("v = 'a\u{7f}b'\n", 1, Syntax);
    fails("v = \"a\rb\"\n", 1, Syntax);
}

#[test]
fn literal_strings() {
    assert_eq!(value(r"'C:\Users\nodejs'"), s(r"C:\Users\nodejs"));
    assert_eq!(value(r#"'<\i\c*\s*>'"#), s(r"<\i\c*\s*>"));
    assert_eq!(value(r#"'Tom "Dubs" Preston'"#), s(r#"Tom "Dubs" Preston"#));
    assert_eq!(value("''"), s(""));
}

#[test]
fn multiline_basic_strings() {
    assert_eq!(value("\"\"\"\nRoses\nViolets\"\"\""), s("Roses\nViolets"));
    assert_eq!(value("\"\"\"\r\none\r\ntwo\"\"\""), s("one\ntwo"));
    assert_eq!(value("\"\"\"\n\nkept\"\"\""), s("\nkept"));
    assert_eq!(value("\"\"\"no newline\"\"\""), s("no newline"));
    assert_eq!(
        value("\"\"\"\nThe quick \\\n\n\n  brown \\\n   fox.\"\"\""),
        s("The quick brown fox.")
    );
    assert_eq!(value("\"\"\"a \\   \n  b\"\"\""), s("a b"));
    assert_eq!(value("\"\"\"tab\\tand \\u0041\"\"\""), s("tab\tand A"));
    assert_eq!(value("\"\"\"a\"\"\"\""), s("a\""));
    assert_eq!(value("\"\"\"a\"\"\"\"\""), s("a\"\""));
    assert_eq!(value("\"\"\"\"\"\"\""), s("\""));
    assert_eq!(value("\"\"\"say \"\"hi\"\" now\"\"\""), s("say \"\"hi\"\" now"));
    assert_eq!(value("\"\"\"\"\"\""), s(""));
}

#[test]
fn multiline_basic_string_errors() {
    fails("v = \"\"\"a\"\"\"\"\"\"\n", 1, Syntax);
    fails("v = \"\"\"\nfine\n\\q\"\"\"\n", 3, Escape);
    fails("v = \"\"\"\nnever\nclosed\n", 4, Syntax);
    fails("v = \"\"\"a \\ b\"\"\"\n", 1, Escape);
    fails("v = \"\"\"\nx\ry\"\"\"\n", 2, Syntax);
    fails("v = \"\"\"\na\u{0}\"\"\"\n", 2, Syntax);
}

#[test]
fn multiline_literal_strings() {
    assert_eq!(value("'''\nfirst\n  second\n'''"), s("first\n  second\n"));
    assert_eq!(value(r"'''I [dw]on't need \d{2} apples'''"), s(r"I [dw]on't need \d{2} apples"));
    assert_eq!(value("'''a\\\n  b'''"), s("a\\\n  b"));
    assert_eq!(value("''''one quote''''"), s("'one quote'"));
    assert_eq!(value("'''''two'''''"), s("''two''"));
    fails("v = '''a''''''\n", 1, Syntax);
    fails("v = '''\nopen\n", 3, Syntax);
}
