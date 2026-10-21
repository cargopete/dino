mod common;
use common::*;
use fixture::ErrorKind::*;

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
fn dotted_keys() {
    assert_eq!(
        ok("a . \"b.c\" . 'd' = 1\na.x = 2\n"),
        doc(vec![(
            "a",
            table(vec![("b.c", table(vec![("d", i(1))])), ("x", i(2))])
        )])
    );
    assert_eq!(
        ok("3.14159 = \"pi\"\n"),
        doc(vec![("3", table(vec![("14159", s("pi"))]))])
    );
}

#[test]
fn headers() {
    let src = "\
# comment line
top = 0

[ owner ]   # comment
name = \"Tom\"

[database.\"conn pool\"]
size = 5

[a.b.c]
[a]
x = 1
[a.b]
y = 2
";
    assert_eq!(
        ok(src),
        doc(vec![
            ("top", i(0)),
            ("owner", table(vec![("name", s("Tom"))])),
            (
                "database",
                table(vec![("conn pool", table(vec![("size", i(5))]))])
            ),
            (
                "a",
                table(vec![
                    ("x", i(1)),
                    ("b", table(vec![("y", i(2)), ("c", table(vec![]))])),
                ])
            ),
        ])
    );
}

#[test]
fn empty_and_implicit_tables() {
    assert_eq!(ok("[x]\n"), doc(vec![("x", table(vec![]))]));
    assert_eq!(
        ok("[x.y.z]\n"),
        doc(vec![("x", table(vec![("y", table(vec![("z", table(vec![]))]))]))])
    );
}

#[test]
fn arrays_of_tables() {
    let src = "\
[[products]]
name = \"Hammer\"
sku = 738594937

[[products]]

[[products]]
name = \"Nail\"
color = \"gray\"
";
    assert_eq!(
        ok(src),
        doc(vec![(
            "products",
            arr(vec![
                table(vec![("name", s("Hammer")), ("sku", i(738_594_937))]),
                table(vec![]),
                table(vec![("name", s("Nail")), ("color", s("gray"))]),
            ])
        )])
    );
}

#[test]
fn nested_arrays_of_tables() {
    let src = "\
[[fruits]]
name = \"apple\"

[fruits.physical]
color = \"red\"

[[fruits.varieties]]
name = \"red delicious\"

[[fruits.varieties]]
name = \"granny smith\"

[[fruits]]
name = \"banana\"

[[fruits.varieties]]
name = \"plantain\"
";
    assert_eq!(
        ok(src),
        doc(vec![(
            "fruits",
            arr(vec![
                table(vec![
                    ("name", s("apple")),
                    ("physical", table(vec![("color", s("red"))])),
                    (
                        "varieties",
                        arr(vec![
                            table(vec![("name", s("red delicious"))]),
                            table(vec![("name", s("granny smith"))]),
                        ])
                    ),
                ]),
                table(vec![
                    ("name", s("banana")),
                    ("varieties", arr(vec![table(vec![("name", s("plantain"))])])),
                ]),
            ])
        )])
    );
}

#[test]
fn sub_tables_of_dotted_tables() {
    let src = "\
[fruit]
apple.color = \"red\"
apple.taste.sweet = true

[fruit.apple.texture]
smooth = true
";
    assert_eq!(
        ok(src),
        doc(vec![(
            "fruit",
            table(vec![(
                "apple",
                table(vec![
                    ("color", s("red")),
                    ("taste", table(vec![("sweet", fixture::Value::Boolean(true))])),
                    ("texture", table(vec![("smooth", fixture::Value::Boolean(true))])),
                ])
            )])
        )])
    );
}

#[test]
fn redefinitions() {
    fails("a = 1\na = 2\n", 2, Redefinition);
    fails("a = 1\n\"a\" = 2\n", 2, Redefinition);
    fails("a = 1\na.b = 2\n", 2, Redefinition);
    fails("[a]\n[a]\n", 2, Redefinition);
    fails("[a]\nb = 1\n[a.b]\n", 3, Redefinition);
    fails("[a.b]\n[a]\n[a]\n", 3, Redefinition);
    fails("[fruit]\napple.color = \"red\"\n[fruit.apple]\n", 3, Redefinition);
    fails("[fruit]\napple.taste.sweet = true\n[fruit.apple.taste]\n", 3, Redefinition);
    fails("a.b = 1\n[a]\n", 2, Redefinition);
    fails("[a.b.c]\nz = 9\n[a]\nb.c.t = 9\n", 4, Redefinition);
    fails("[a]\nb.c = 1\n[a.d]\n[a]\n", 4, Redefinition);
    fails("[x]\n[x.y]\n[x]\ny.z = 1\n", 3, Redefinition);
    fails("[a]\n[[a]]\n", 2, Redefinition);
    fails("[[a]]\n[a]\n", 2, Redefinition);
    fails("[[a]]\nb = 1\n[[a]]\nb = 1\nb = 2\n", 5, Redefinition);
}

#[test]
fn a_dotted_key_from_another_section_cannot_extend_a_table() {
    fails("[a]\nb.c = 1\n[x]\n[a.b]\nd = 1\n", 4, Redefinition);
    assert_eq!(
        ok("[a]\nb.c = 1\nb.d = 2\n"),
        doc(vec![("a", table(vec![("b", table(vec![("c", i(1)), ("d", i(2))]))]))])
    );
}

#[test]
fn a_redefinition_is_reported_at_the_key_before_the_value() {
    fails("a = 1\na = \"\\q\"\n", 2, Redefinition);
    fails("[t]\na.b = 1\n\n\na.b = \"\"\"\n\\q\"\"\"\n", 5, Redefinition);
}

#[test]
fn header_syntax() {
    fails("[]\n", 1, Syntax);
    fails("[a\n", 1, Syntax);
    fails("[a]]\n", 1, Syntax);
    fails("[[a]\n", 1, Syntax);
    fails("[[a] ]\n", 1, Syntax);
    fails("[ [a]]\n", 1, Syntax);
    fails("[a.]\n", 1, Syntax);
    fails("[a b]\n", 1, Syntax);
    fails("[a] x = 1\n", 1, Syntax);
    fails("ok = 1\n\n\n[a.b c]\n", 4, Syntax);
}

#[test]
fn key_syntax() {
    fails("= 1\n", 1, Syntax);
    fails("a b = 1\n", 1, Syntax);
    fails("a. = 1\n", 1, Syntax);
    fails(".a = 1\n", 1, Syntax);
    fails("a\n= 1\n", 1, Syntax);
    fails("a = 1 b = 2\n", 1, Syntax);
    fails("a.b.c\n", 1, Syntax);
    fails("é = 1\n", 1, Syntax);
    fails("\"a\nb\" = 1\n", 1, Syntax);
}
