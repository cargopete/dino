use fixture::*;

fn texts(src: &str) -> Vec<(TokenKind, String)> {
    tokenize(src)
        .unwrap()
        .iter()
        .map(|t| (t.kind, t.as_str().to_owned()))
        .collect()
}

#[test]
fn tokenizes_an_entry() {
    use TokenKind::*;
    let got = texts("name = \"gyr\"; # trailing comment\nsize = [1, 2.5];");
    let want = [
        (Ident, "name"),
        (Equals, "="),
        (Str, "gyr"),
        (Semicolon, ";"),
        (Ident, "size"),
        (Equals, "="),
        (LBracket, "["),
        (Number, "1"),
        (Comma, ","),
        (Number, "2.5"),
        (RBracket, "]"),
        (Semicolon, ";"),
    ];
    let want: Vec<(TokenKind, String)> = want.iter().map(|(k, s)| (*k, s.to_string())).collect();
    assert_eq!(got, want);
}

#[test]
fn spans_are_line_and_column() {
    let tokens = tokenize("a = 1;\n  bb = 2;").unwrap();
    assert_eq!(tokens[0].span, Span { line: 1, col: 1 });
    assert_eq!(tokens[4].span, Span { line: 2, col: 3 });
    assert_eq!(tokens[6].span, Span { line: 2, col: 8 });
}

#[test]
fn lex_errors() {
    assert_eq!(
        tokenize("a = $"),
        Err(LexError::UnexpectedChar { ch: '$', span: Span { line: 1, col: 5 } })
    );
    assert_eq!(
        tokenize("a = \"open"),
        Err(LexError::UnterminatedString { span: Span { line: 1, col: 5 } })
    );
}

#[test]
fn parses_and_queries() {
    let entries = parse_str("name = \"a\\tb\";\nlist = [x, 2, \"y\",];\nk = v;").unwrap();
    assert_eq!(keys(&entries), vec!["name", "list", "k"]);
    assert_eq!(get_str(&entries, "name"), Some("a\tb".to_owned()));
    assert_eq!(get(&entries, "k").and_then(|v| v.as_str()), Some("v"));
    match get(&entries, "list") {
        Some(Value::List(items)) => {
            let items: Vec<Option<&str>> = items.iter().map(|v| v.as_str()).collect();
            assert_eq!(items, vec![Some("x"), Some("2"), Some("y")]);
        }
        other => panic!("not a list: {other:?}"),
    }
}

#[test]
fn parse_errors() {
    assert!(matches!(
        parse_str("a = ;"),
        Err(Error::Parse(ParseError::Expected { expected: "a value", .. }))
    ));
    assert_eq!(
        parse_str("a = 1"),
        Err(Error::Parse(ParseError::UnexpectedEof { expected: "`;`" }))
    );
    assert!(matches!(
        parse_str("a = 1; a = 2;"),
        Err(Error::Parse(ParseError::DuplicateKey { .. }))
    ));
}

#[test]
fn summaries() {
    let tokens = tokenize("ab = cd; xyz = [p, q];").unwrap();
    assert_eq!(
        count_kinds(&tokens),
        vec![
            (TokenKind::Ident, 5),
            (TokenKind::Equals, 2),
            (TokenKind::Semicolon, 2),
            (TokenKind::LBracket, 1),
            (TokenKind::Comma, 1),
            (TokenKind::RBracket, 1),
        ]
    );
    assert_eq!(longest_ident(&tokens), Some("xyz"));
}
