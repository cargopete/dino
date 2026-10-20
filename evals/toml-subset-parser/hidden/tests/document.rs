mod common;
use common::*;
use fixture::ErrorKind::*;
use fixture::Value::Boolean;

#[test]
fn an_empty_document() {
    assert_eq!(ok(""), doc(vec![]));
    assert_eq!(ok("\n\n  \t\n# only a comment"), doc(vec![]));
    assert_eq!(ok("a = 1"), doc(vec![("a", i(1))]));
}

#[test]
fn crlf_and_whitespace() {
    let src = "a = 1\r\n\t[ t ]\t# c\r\n  b\t=\t\"x\"  \r\n";
    assert_eq!(
        ok(src),
        doc(vec![("a", i(1)), ("t", table(vec![("b", s("x"))]))])
    );
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
fn a_realistic_document() {
    let src = r#"# This is a TOML document

title = "TOML Example"

[owner]
name = "Tom Preston-Werner"
"e-mail" = 'tom@example.com'

[database]
enabled = true
ports = [ 8000, 8001, 8002 ]
data = [ ["delta", "phi"], [3, 0x0e] ]
limits = { cpu = 4, memory.mb = 2_048 }

[servers]

  [servers.alpha]
  ip = "10.0.0.1"
  role = """
frontend \
  and edge"""

  [servers.beta]
  ip = "10.0.0.2"
  role = '''backend'''

[[servers.beta.disks]]
size = 0o777
[[servers.beta.disks]]
size = -1
"#;
    assert_eq!(
        ok(src),
        doc(vec![
            ("title", s("TOML Example")),
            (
                "owner",
                table(vec![("name", s("Tom Preston-Werner")), ("e-mail", s("tom@example.com"))])
            ),
            (
                "database",
                table(vec![
                    ("enabled", Boolean(true)),
                    ("ports", arr(vec![i(8000), i(8001), i(8002)])),
                    (
                        "data",
                        arr(vec![arr(vec![s("delta"), s("phi")]), arr(vec![i(3), i(14)])])
                    ),
                    (
                        "limits",
                        table(vec![("cpu", i(4)), ("memory", table(vec![("mb", i(2048))]))])
                    ),
                ])
            ),
            (
                "servers",
                table(vec![
                    (
                        "alpha",
                        table(vec![("ip", s("10.0.0.1")), ("role", s("frontend and edge"))])
                    ),
                    (
                        "beta",
                        table(vec![
                            ("ip", s("10.0.0.2")),
                            ("role", s("backend")),
                            (
                                "disks",
                                arr(vec![table(vec![("size", i(511))]), table(vec![("size", i(-1))])])
                            ),
                        ])
                    ),
                ])
            ),
        ])
    );
}

#[test]
fn errors_are_reported_on_the_right_line() {
    let src = "a = 1\nb = \"\"\"\none\ntwo\"\"\"\n[t]\nc = [\n  1,\n  2,\n]\nd = 0x\n";
    fails(src, 10, Syntax);
    let src = "a = 1\n\n\n\n[t]\n\n[u]\n\n[t]\n";
    fails(src, 9, Redefinition);
    fails("x = '''\n\n\n'''\ny = \"\\u00\"\n", 5, Escape);
}

#[test]
fn many_keys_and_deep_tables() {
    let mut src = String::new();
    for n in 0..500 {
        src.push_str(&format!("k{n} = {n}\n"));
    }
    let path: Vec<String> = (0..60).map(|n| format!("t{n}")).collect();
    src.push_str(&format!("[{}]\nleaf = true\n", path.join(".")));
    let parsed = ok(&src);
    assert_eq!(parsed.len(), 501);
    assert_eq!(parsed["k499"], i(499));
    let mut node = &parsed["t0"];
    for key in &path[1..] {
        match node {
            fixture::Value::Table(inner) => node = &inner[key.as_str()],
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(node, &table(vec![("leaf", Boolean(true))]));
}
