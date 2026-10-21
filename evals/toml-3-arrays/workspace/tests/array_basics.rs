use fixture::{parse, Value};

#[test]
fn arrays() {
    let doc = parse("ports = [ 80, 443, ]\n").unwrap();
    assert_eq!(
        doc["ports"],
        Value::Array(vec![Value::Integer(80), Value::Integer(443)])
    );
}

#[test]
fn inline_tables() {
    let doc = parse("point = { x = 1, y = 2 }\n").unwrap();
    let Value::Table(point) = &doc["point"] else { panic!("{doc:?}") };
    assert_eq!(point["x"], Value::Integer(1));
    assert_eq!(point["y"], Value::Integer(2));
}
