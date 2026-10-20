use fixture::{parse, ErrorKind, ParseError, Value};

fn get<'a>(table: &'a fixture::Table, path: &[&str]) -> &'a Value {
    let (last, rest) = path.split_last().unwrap();
    let mut table = table;
    for key in rest {
        match &table[*key] {
            Value::Table(inner) => table = inner,
            other => panic!("{key} is not a table: {other:?}"),
        }
    }
    &table[*last]
}

#[test]
fn key_values() {
    let doc = parse("name = \"gyr\"\ncount = 3\nready = true\n").unwrap();
    assert_eq!(doc["name"], Value::String("gyr".into()));
    assert_eq!(doc["count"], Value::Integer(3));
    assert_eq!(doc["ready"], Value::Boolean(true));
}

#[test]
fn tables() {
    let doc = parse("[server]\nport = 8080\n\n[server.tls]\nenabled = false\n").unwrap();
    assert_eq!(get(&doc, &["server", "port"]), &Value::Integer(8080));
    assert_eq!(get(&doc, &["server", "tls", "enabled"]), &Value::Boolean(false));
}

#[test]
fn arrays() {
    let doc = parse("ports = [ 80, 443, ]\n").unwrap();
    assert_eq!(
        doc["ports"],
        Value::Array(vec![Value::Integer(80), Value::Integer(443)])
    );
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
