mod common;
use common::*;
use fixture::ErrorKind::*;

#[test]
fn an_empty_document() {
    assert_eq!(ok(""), doc(vec![]));
    assert_eq!(ok("\n\n  \t\n# only a comment"), doc(vec![]));
    assert_eq!(ok("a = 1"), doc(vec![("a", i(1))]));
}

#[test]
fn crlf_and_whitespace() {
    let src = "a = 1\r\n\t# c\r\n  b\t=\t\"x\"  # d\r\n";
    assert_eq!(ok(src), doc(vec![("a", i(1)), ("b", s("x"))]));
}

#[test]
fn line_ends() {
    fails("a = 1\r", 1, Syntax);
    fails("a = 1\rb = 2\n", 1, Syntax);
    fails("a = 1\n\r\n\rb = 2\n", 3, Syntax);
    fails("# fine\n# bad \u{1} char\n", 2, Syntax);
    fails("a = 1 # tab\tis fine\nb = 2 # but not \u{7f}\n", 2, Syntax);
    fails("a = 1\n\u{c}\n", 2, Syntax);
}

#[test]
fn comments_may_hold_anything_printable() {
    assert_eq!(
        ok("a = \"#\" # \"quoted\" [x] = é 😀 ''' \"\"\"\n"),
        doc(vec![("a", s("#"))])
    );
}

#[test]
fn keys() {
    assert_eq!(
        ok("bare_key-1 = 1\n1234 = 2\n\"quoted key\" = 3\n'lit.key' = 4\n\"\" = 5\n"),
        doc(vec![
            ("bare_key-1", i(1)),
            ("1234", i(2)),
            ("quoted key", i(3)),
            ("lit.key", i(4)),
            ("", i(5)),
        ])
    );
    assert_eq!(ok("\"\\u0041\" = 1\n"), doc(vec![("A", i(1))]));
    assert_eq!(ok("true = 1\n"), doc(vec![("true", i(1))]));
}

#[test]
fn key_syntax() {
    fails("= 1\n", 1, Syntax);
    fails("a b = 1\n", 1, Syntax);
    fails("a\n= 1\n", 1, Syntax);
    fails("a = 1 b = 2\n", 1, Syntax);
    fails("é = 1\n", 1, Syntax);
    fails("\"a\nb\" = 1\n", 1, Syntax);
    fails("ok = 1\n\nkey\n", 3, Syntax);
}

#[test]
fn redefinitions() {
    fails("a = 1\na = 2\n", 2, Redefinition);
    fails("a = 1\n\"a\" = 2\n", 2, Redefinition);
    fails("a = 1\nb = 2\n'a' = 3\n", 3, Redefinition);
    fails("a = 1\na = \"\\q\"\n", 2, Redefinition);
}

#[test]
fn errors_are_reported_on_the_right_line() {
    fails("a = 1\nb = \"\"\"\none\ntwo\"\"\"\nd = 0x\n", 5, Syntax);
    fails("x = '''\n\n\n'''\ny = \"\\u00\"\n", 5, Escape);
    fails("a = 1\n\n\n\nb = 99999999999999999999\n", 5, Integer);
}

#[test]
fn many_keys() {
    let mut src = String::new();
    for n in 0..500 {
        src.push_str(&format!("k{n} = {n}\n"));
    }
    let parsed = ok(&src);
    assert_eq!(parsed.len(), 500);
    assert_eq!(parsed["k499"], i(499));
}
