mod common;
use common::*;
use fixture::ErrorKind::*;
use fixture::Value::Boolean;

#[test]
fn arrays() {
    assert_eq!(value("[]"), arr(vec![]));
    assert_eq!(value("[ ]"), arr(vec![]));
    assert_eq!(value("[1]"), arr(vec![i(1)]));
    assert_eq!(value("[1,2,]"), arr(vec![i(1), i(2)]));
    assert_eq!(
        value("[ \"a\", 'b', 1, true, [2, [3]], {x = 1} ]"),
        arr(vec![
            s("a"),
            s("b"),
            i(1),
            Boolean(true),
            arr(vec![i(2), arr(vec![i(3)])]),
            table(vec![("x", i(1))]),
        ])
    );
}

#[test]
fn arrays_across_lines_with_comments() {
    let src = "v = [  # start\n  1,\n\n  # a comment line\n  2 # after a value\n  , 3,\n  ]   # end\nw = 4\n";
    assert_eq!(
        ok(src),
        doc(vec![("v", arr(vec![i(1), i(2), i(3)])), ("w", i(4))])
    );
    assert_eq!(
        value("[\r\n1,\r\n\"\"\"\ntwo\nlines\"\"\",\r\n]"),
        arr(vec![i(1), s("two\nlines")])
    );
}

#[test]
fn array_errors() {
    fails("v = [,]\n", 1, Syntax);
    fails("v = [1,,2]\n", 1, Syntax);
    fails("v = [1 2]\n", 1, Syntax);
    fails("v = [1\n", 2, Syntax);
    fails("v = [\n1,\n2\n", 4, Syntax);
    fails("v = [\n1,\n2.0]\n", 3, Syntax);
    fails("v = [\n\"a\",\n\"\\z\"]\n", 3, Escape);
    fails("v = [\n0,\n99999999999999999999]\n", 3, Integer);
    fails("v = [1]]\n", 1, Syntax);
    fails("v = [1 # comment ]\n", 2, Syntax);
}

#[test]
fn inline_tables() {
    assert_eq!(value("{}"), table(vec![]));
    assert_eq!(value("{ }"), table(vec![]));
    assert_eq!(
        value("{ first = \"Tom\", last = \"Preston-Werner\" }"),
        table(vec![("first", s("Tom")), ("last", s("Preston-Werner"))])
    );
    assert_eq!(
        value("{x=1,y=2}"),
        table(vec![("x", i(1)), ("y", i(2))])
    );
    assert_eq!(
        value("{ type.name = \"pug\", type.size = 3, \"q k\" = { a = [1, 2] } }"),
        table(vec![
            ("type", table(vec![("name", s("pug")), ("size", i(3))])),
            ("q k", table(vec![("a", arr(vec![i(1), i(2)]))])),
        ])
    );
    assert_eq!(
        value("{ a = [\n1,\n2], b = \"\"\"x\ny\"\"\" }"),
        table(vec![("a", arr(vec![i(1), i(2)])), ("b", s("x\ny"))])
    );
}

#[test]
fn inline_tables_in_sections() {
    let src = "[point]\nxy = { x = 1, y = 2 }\nname.first = { v = 1 }\nname.last = 2\n";
    assert_eq!(
        ok(src),
        doc(vec![(
            "point",
            table(vec![
                ("xy", table(vec![("x", i(1)), ("y", i(2))])),
                (
                    "name",
                    table(vec![("first", table(vec![("v", i(1))])), ("last", i(2))])
                ),
            ])
        )])
    );
}

#[test]
fn inline_table_errors() {
    fails("v = { a = 1, }\n", 1, Syntax);
    fails("v = { a = 1\n}\n", 1, Syntax);
    fails("v = {\na = 1 }\n", 1, Syntax);
    fails("v = { a = 1 b = 2 }\n", 1, Syntax);
    fails("v = { a = 1,, b = 2 }\n", 1, Syntax);
    fails("v = { a = 1 }}\n", 1, Syntax);
    fails("v = { a = 1, a = 2 }\n", 1, Redefinition);
    fails("v = { a.b = 1, a = 2 }\n", 1, Redefinition);
    fails("v = { a = { b = 1 }, a.c = 2 }\n", 1, Redefinition);
    fails("v = { a = 1, a.b = 2 }\n", 1, Redefinition);
    fails("v = { a = [\n1], a = 2 }\n", 2, Redefinition);
    fails("v = { a = \"\\e\" }\n", 1, Escape);
    fails("v = {a=1}\nv.b = 2\n", 2, Redefinition);
    fails("v = {a=1}\n[v]\n", 2, Redefinition);
}

#[test]
fn inline_tables_nest_dotted_keys_independently() {
    assert_eq!(
        value("{ a = { b.c = 1 }, d = { b.c = 2 } }"),
        table(vec![
            ("a", table(vec![("b", table(vec![("c", i(1))]))])),
            ("d", table(vec![("b", table(vec![("c", i(2))]))])),
        ])
    );
}
