import json, re, sys
o = json.load(open(sys.argv[1])); out = sys.argv[2]
def ident(s, i):
    return f"{i:03}"
V = o["versions"]
lines = ["//! Generated from node-semver 7.6.3, the reference implementation of these rules.", "", "use fixture::{Range, Version};", "",
"const VERSIONS: &str = \"" + " ".join(V) + "\";", "",
"""fn matching(range: &str) -> String {
    let range = Range::parse(range).unwrap_or_else(|_| panic!("{range:?} should parse"));
    VERSIONS
        .split(' ')
        .filter(|v| range.matches(&Version::parse(v).unwrap()))
        .collect::<Vec<_>>()
        .join(" ")
}
""",
"macro_rules! matches {\n    ($name:ident, $range:expr, $want:expr) => {\n        #[test]\n        fn $name() {\n            assert_eq!(matching($range), $want, \"range {:?}\", $range);\n        }\n    };\n}\n",
"macro_rules! invalid {\n    ($name:ident, $range:expr) => {\n        #[test]\n        fn $name() {\n            assert!(Range::parse($range).is_err(), \"{:?} should not parse\", $range);\n        }\n    };\n}\n"]
counts = {}
for r in o["ranges"]:
    g = r["group"]; counts[g] = counts.get(g, 0) + 1
    name = f"{g}_{counts[g]:02}"
    if r["valid"]:
        lines.append(f"matches!({name}, {json.dumps(r['range'])}, {json.dumps(' '.join(r['matches']))});")
    else:
        lines.append(f"invalid!({name}, {json.dumps(r['range'])});")
open(f"{out}/ranges.rs", "w").write("\n".join(lines) + "\n")

L = ["//! Generated from node-semver 7.6.3.", "", "use std::cmp::Ordering;", "", "use fixture::{Range, Version};", ""]
for i, p in enumerate(o["parse"]):
    n = f"parse_{i+1:02}"
    if p["valid"]:
        L.append(f"#[test]\nfn {n}() {{\n    let v = Version::parse({json.dumps(p['input'])}).expect(\"valid\");\n    assert_eq!(v.to_string(), {json.dumps(p['display'])});\n}}\n")
    else:
        L.append(f"#[test]\nfn {n}() {{\n    assert!(Version::parse({json.dumps(p['input'])}).is_err());\n}}\n")
ords = {-1: "Less", 0: "Equal", 1: "Greater"}
lefts = []
for a, b, c in o["compare"]:
    if a not in lefts: lefts.append(a)
for i, a in enumerate(lefts):
    rows = [(b, c) for x, b, c in o["compare"] if x == a]
    body = "\n".join(f"    assert_eq!(cmp({json.dumps(a)}, {json.dumps(b)}), Ordering::{ords[c]}, \"{a} vs {b}\");" for b, c in rows)
    L.append(f"#[test]\nfn order_{i+1:02}() {{\n{body}\n}}\n")
L.insert(6, "fn cmp(a: &str, b: &str) -> Ordering {\n    Version::parse(a).unwrap().cmp(&Version::parse(b).unwrap())\n}\n")
for i, m in enumerate(o["max"]):
    vs = ", ".join(json.dumps(v) for v in m["versions"])
    def opt(x): return "None" if x is None else f"Some({json.dumps(x)}.to_owned())"
    L.append(f"""#[test]
fn satisfying_{i+1:02}() {{
    let versions: Vec<Version> = [{vs}].iter().map(|v| Version::parse(v).unwrap()).collect();
    let range = Range::parse({json.dumps(m['range'])}).unwrap();
    assert_eq!(range.max_satisfying(&versions).map(ToString::to_string), {opt(m['max'])});
    assert_eq!(range.min_satisfying(&versions).map(ToString::to_string), {opt(m['min'])});
}}
""")
open(f"{out}/versions.rs", "w").write("\n".join(L) + "\n")
