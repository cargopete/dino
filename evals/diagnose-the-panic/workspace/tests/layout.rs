use fixture::{pad, render_table, truncate, wrap, Align};

#[test]
fn wraps_at_word_boundaries() {
    assert_eq!(
        wrap("the quick brown fox jumps over the lazy dog", 12),
        vec!["the quick", "brown fox", "jumps over", "the lazy dog"]
    );
}

#[test]
fn collapses_runs_of_whitespace() {
    assert_eq!(wrap("  one \t two\n\nthree  ", 20), vec!["one two three"]);
    assert!(wrap("   ", 10).is_empty());
}

#[test]
fn splits_words_longer_than_the_width() {
    assert_eq!(
        wrap("go abcdefghij x", 4),
        vec!["go", "abcd", "efgh", "ij x"]
    );
}

#[test]
fn truncates_with_an_ellipsis() {
    assert_eq!(truncate("hello world", 8), "hello w…");
    assert_eq!(truncate("hello", 5), "hello");
    assert_eq!(truncate("hello", 1), "…");
    assert_eq!(truncate("hello", 0), "");
}

#[test]
fn pads_each_way() {
    assert_eq!(pad("ab", 6, Align::Left), "ab    ");
    assert_eq!(pad("ab", 6, Align::Right), "    ab");
    assert_eq!(pad("ab", 7, Align::Center), "  ab   ");
    assert_eq!(pad("toolong", 3, Align::Left), "toolong");
}

#[test]
fn renders_a_table() {
    let rows = vec![
        vec!["apple".to_string(), "3".to_string()],
        vec!["kiwi".to_string(), "12".to_string()],
    ];
    let table = render_table(&["fruit", "qty"], &rows, &[Align::Left, Align::Right], 10);
    assert_eq!(
        table,
        "fruit | qty\n\
         ------+----\n\
         apple |   3\n\
         kiwi  |  12"
    );
}

#[test]
fn table_truncates_wide_cells() {
    let rows = vec![vec!["a very long description".to_string()]];
    let table = render_table(&["desc"], &rows, &[Align::Left], 8);
    assert_eq!(table, "desc    \n--------\na very …");
}
