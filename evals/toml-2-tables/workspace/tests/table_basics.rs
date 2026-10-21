use fixture::{parse, ErrorKind, ParseError, Table, Value};

fn get<'a>(table: &'a Table, path: &[&str]) -> &'a Value {
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
fn tables() {
    let doc = parse("[server]\nport = 8080\n\n[server.tls]\nenabled = false\n").unwrap();
    assert_eq!(get(&doc, &["server", "port"]), &Value::Integer(8080));
    assert_eq!(get(&doc, &["server", "tls", "enabled"]), &Value::Boolean(false));
}

#[test]
fn dotted_keys() {
    let doc = parse("a.b = 1\na.c = 2\n").unwrap();
    assert_eq!(get(&doc, &["a", "b"]), &Value::Integer(1));
    assert_eq!(get(&doc, &["a", "c"]), &Value::Integer(2));
}

#[test]
fn errors() {
    assert_eq!(
        parse("[a]\n[a]\n"),
        Err(ParseError { line: 2, kind: ErrorKind::Redefinition })
    );
}
