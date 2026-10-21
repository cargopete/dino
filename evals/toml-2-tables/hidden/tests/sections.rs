mod common;
use common::*;
use fixture::ErrorKind::*;
use fixture::Value::Boolean;

#[test]
fn crlf_and_whitespace_around_headers() {
    let src = "a = 1\r\n\t[ t ]\t# c\r\n  b\t=\t\"x\"  \r\n[[ u . v ]]\r\n";
    assert_eq!(
        ok(src),
        doc(vec![
            ("a", i(1)),
            ("t", table(vec![("b", s("x"))])),
            ("u", table(vec![("v", arr(vec![table(vec![])]))])),
        ])
    );
}

#[test]
fn a_document_of_sections() {
    let src = r#"# This is a TOML document

title = "TOML Example"

[owner]
name = "Tom Preston-Werner"
"e-mail" = 'tom@example.com'

[database]
enabled = true
limits.cpu = 4
limits.memory.mb = 2_048

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
    fails("a = 1\n\n\n\n[t]\n\n[u]\n\n[t]\n", 9, Redefinition);
    fails("[t]\nb = \"\"\"\none\ntwo\"\"\"\n[u.\n", 5, Syntax);
    fails("[a]\nx = 1\n\n[[a.b]]\n[a.b.c]\n[a.b.c]\n", 6, Redefinition);
}

#[test]
fn deep_tables() {
    let path: Vec<String> = (0..60).map(|n| format!("t{n}")).collect();
    let src = format!("[{}]\nleaf = true\n", path.join("."));
    let parsed = ok(&src);
    let mut node = &parsed["t0"];
    for key in &path[1..] {
        match node {
            fixture::Value::Table(inner) => node = &inner[key.as_str()],
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(node, &table(vec![("leaf", Boolean(true))]));
}
