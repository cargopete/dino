use fixture::*;

// Each of these compiles only if the returned text borrows from `src`, not
// from the token or entry vectors that are dropped at the end of the body.

fn first_text(src: &str) -> &str {
    tokenize(src).unwrap()[0].text
}

fn first_as_str(src: &str) -> &str {
    tokenize(src).unwrap()[0].as_str()
}

fn first_token(src: &str) -> Token<'_> {
    Lexer::new(src).next().unwrap().unwrap()
}

fn entries_via_tokens(src: &str) -> Vec<Entry<'_>> {
    let tokens = tokenize(src).unwrap();
    parse(&tokens).unwrap()
}

fn keys_of(src: &str) -> Vec<&str> {
    let entries = parse_str(src).unwrap();
    keys(&entries)
}

fn first_value(src: &str) -> Option<&str> {
    let entries = parse_str(src).unwrap();
    entries[0].value.as_str()
}

fn looked_up<'s>(src: &'s str, key: &str) -> Option<&'s str> {
    let entries = parse_str(src).unwrap();
    get(&entries, key).and_then(|v| v.as_str())
}

fn longest(src: &str) -> Option<&str> {
    let tokens = tokenize(src).unwrap();
    longest_ident(&tokens)
}

fn found_token(src: &str) -> Token<'_> {
    match parse_str(src) {
        Err(Error::Parse(ParseError::Expected { found, .. })) => found,
        other => panic!("expected a parse error, got {other:?}"),
    }
}

fn duplicate_key(src: &str) -> &str {
    let tokens = tokenize(src).unwrap();
    match parse(&tokens) {
        Err(ParseError::DuplicateKey { key, .. }) => key,
        other => panic!("expected a duplicate key, got {other:?}"),
    }
}

fn offset_in(src: &str, s: &str) -> usize {
    let lo = src.as_ptr() as usize;
    let p = s.as_ptr() as usize;
    assert!(
        p >= lo && p + s.len() <= lo + src.len(),
        "{s:?} does not point into the source"
    );
    p - lo
}

#[test]
fn texts_point_into_the_source() {
    let src = String::from("größe = \"a\\\"b\";");
    assert_eq!(first_text(&src), "größe");
    assert_eq!(offset_in(&src, first_text(&src)), 0);
    assert_eq!(offset_in(&src, first_as_str(&src)), 0);
    let tok = first_token(&src);
    assert_eq!(tok, Token::new(TokenKind::Ident, &src[..7], Span { line: 1, col: 1 }));

    let entries = entries_via_tokens(&src);
    assert_eq!(entries[0].key, "größe");
    assert_eq!(offset_in(&src, entries[0].key), 0);
    match &entries[0].value {
        Value::Str(raw) => {
            assert_eq!(*raw, "a\\\"b");
            assert_eq!(offset_in(&src, raw), 11);
        }
        other => panic!("not a string: {other:?}"),
    }
}

#[test]
fn queries_return_slices_of_the_source() {
    let src = String::from("alpha = one;\nβeta = \"two\";\ngamma = [3];");
    let ks = keys_of(&src);
    assert_eq!(ks, vec!["alpha", "βeta", "gamma"]);
    assert_eq!(offset_in(&src, ks[1]), 13);
    let v = first_value(&src).unwrap();
    assert_eq!((v, offset_in(&src, v)), ("one", 8));
    let two = looked_up(&src, "βeta").unwrap();
    assert_eq!((two, offset_in(&src, two)), ("two", 22));
    assert_eq!(looked_up(&src, "gamma"), None);
    let l = longest(&src).unwrap();
    assert_eq!((l, offset_in(&src, l)), ("alpha", 0));
}

#[test]
fn errors_borrow_too() {
    let src = String::from("é = = 1;");
    let found = found_token(&src);
    assert_eq!(found.kind, TokenKind::Equals);
    assert_eq!(found.span, Span { line: 1, col: 5 });
    assert_eq!(offset_in(&src, found.text), 5);

    let src = String::from("k = 1;\nk = 2;");
    let key = duplicate_key(&src);
    assert_eq!((key, offset_in(&src, key)), ("k", 7));
    assert_eq!(
        parse_str(&src),
        Err(Error::Parse(ParseError::DuplicateKey {
            key: &src[7..8],
            first: Span { line: 1, col: 1 },
            second: Span { line: 2, col: 1 },
        }))
    );
}

#[test]
fn empty_string_literal_is_still_a_slice() {
    let src = String::from("e = \"\";");
    let tokens = tokenize(&src).unwrap();
    assert_eq!(tokens[2].kind, TokenKind::Str);
    assert_eq!(tokens[2].text, "");
    assert_eq!(offset_in(&src, tokens[2].text), 5);
}

#[test]
fn tokens_are_cheap_to_copy() {
    let src = String::from("a = b;");
    let tokens = tokenize(&src).unwrap();
    let copy: Token<'_> = tokens[0];
    assert_eq!(copy, tokens[0]);
    assert_eq!(copy.text.as_ptr(), src.as_ptr());
}
