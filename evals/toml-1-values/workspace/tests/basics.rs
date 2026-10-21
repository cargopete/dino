use fixture::{parse, ErrorKind, ParseError, Value};

#[test]
fn key_values() {
    let doc = parse("name = \"gyr\"\ncount = 3\nready = true\n").unwrap();
    assert_eq!(doc["name"], Value::String("gyr".into()));
    assert_eq!(doc["count"], Value::Integer(3));
    assert_eq!(doc["ready"], Value::Boolean(true));
}

#[test]
fn comments_and_blank_lines() {
    let doc = parse("# config\n\nport = 0x1F90 # hex\n").unwrap();
    assert_eq!(doc["port"], Value::Integer(8080));
}

#[test]
fn errors() {
    assert_eq!(
        parse("a = 1\na = 2\n"),
        Err(ParseError { line: 2, kind: ErrorKind::Redefinition })
    );
    assert_eq!(
        parse("a = \"\\q\"\n"),
        Err(ParseError { line: 1, kind: ErrorKind::Escape })
    );
}
