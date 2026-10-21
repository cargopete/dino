mod common;
use common::*;
use fixture::ErrorKind::*;
use fixture::Value::Boolean;

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
fn values_complete_as_written() {
    fails("a = {}\n[a]\n", 2, Redefinition);
    fails("a = {}\n[a.b]\n", 2, Redefinition);
    fails("a = { b = 1 }\na.c = 2\n", 2, Redefinition);
    fails("a = [1]\n[[a]]\n", 2, Redefinition);
    fails("a = []\n[a.b]\n", 2, Redefinition);
    fails("a = 1\n\n\na = [\n1,\n2.5]\n", 4, Redefinition);
}

#[test]
fn errors_are_reported_on_the_right_line() {
    let src = "a = 1\nb = \"\"\"\none\ntwo\"\"\"\n[t]\nc = [\n  1,\n  2,\n]\nd = 0x\n";
    fails(src, 10, Syntax);
    fails("[t]\nv = { a = [\n1,\n2], b = '\n' }\n", 4, Syntax);
}
