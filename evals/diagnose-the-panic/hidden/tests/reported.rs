use fixture::{display_width, pad, render_table, truncate, wrap, Align};

fn rows(cells: &[&[&str]]) -> Vec<Vec<String>> {
    cells
        .iter()
        .map(|row| row.iter().map(|c| (*c).to_string()).collect())
        .collect()
}

// The reported symptoms.

#[test]
fn truncate_does_not_panic_on_non_ascii() {
    assert_eq!(truncate("Grüße aus München", 8), "Grüße a…");
    assert_eq!(truncate("日本語のテキスト", 5), "日本語の…");
    assert_eq!(truncate("ab日本語", 3), "ab…");
    assert_eq!(truncate("üü", 1), "…");
    assert_eq!(truncate("ü", 1), "ü");
    assert_eq!(truncate("éé", 0), "");
}

#[test]
fn truncate_counts_chars_not_bytes() {
    // Cuts that land on a char boundary by luck must still keep max - 1 chars.
    assert_eq!(truncate("жжжжжж", 5), "жжжж…");
    assert_eq!(display_width(&truncate("Ωmega Ωmega", 4)), 4);
}

#[test]
fn wrap_splits_long_non_ascii_words_by_char() {
    assert_eq!(wrap("Ärger", 2), vec!["Är", "ge", "r"]);
    assert_eq!(wrap("x ŻŻŻŻŻ y", 2), vec!["x", "ŻŻ", "ŻŻ", "Ż", "y"]);
    assert_eq!(wrap("日本語日本語", 4), vec!["日本語日", "本語"]);
    assert_eq!(wrap("ббб", 1), vec!["б", "б", "б"]);
    assert_eq!(wrap("x ŻŻŻŻŻ y", 4), vec!["x", "ŻŻŻŻ", "Ż y"]);
}

#[test]
fn wrapped_lines_never_exceed_the_width() {
    assert_eq!(wrap("aaa bb", 5), vec!["aaa", "bb"]);
    assert_eq!(wrap("aaa bb", 6), vec!["aaa bb"]);
    assert_eq!(wrap("one two three four", 8), vec!["one two", "three", "four"]);
    let text = "a bb ccc dddd eeeee ffffff ggggggg hhhhhhhh";
    for width in 1..20 {
        for line in wrap(text, width) {
            assert!(display_width(&line) <= width, "{line:?} wider than {width}");
        }
    }
}

#[test]
fn pad_counts_chars_not_bytes() {
    assert_eq!(pad("café", 6, Align::Left), "café  ");
    assert_eq!(pad("café", 6, Align::Right), "  café");
    assert_eq!(pad("ñ", 4, Align::Center), " ñ  ");
    assert_eq!(pad("日本", 4, Align::Right), "  日本");
    assert_eq!(pad("日本", 2, Align::Left), "日本");
}

#[test]
fn table_columns_line_up_with_non_ascii_text() {
    let body = rows(&[&["Zürich", "421"], &["Genève", "203"], &["Bern", "134"]]);
    let table = render_table(&["city", "pop"], &body, &[Align::Left, Align::Right], 10);
    assert_eq!(
        table,
        "city   | pop\n\
         -------+----\n\
         Zürich | 421\n\
         Genève | 203\n\
         Bern   | 134"
    );
}

#[test]
fn table_truncates_non_ascii_cells() {
    let body = rows(&[&["東京都千代田区丸の内", "ok"]]);
    let table = render_table(&["名前", "s"], &body, &[Align::Left, Align::Center], 5);
    assert_eq!(table, "名前    | s \n------+---\n東京都千… | ok");
}

#[test]
fn every_table_line_has_the_same_width() {
    let body = rows(&[
        &["Ελλάδα", "Αθήνα", "10.4"],
        &["España", "Madrid", "47.4"],
        &["Россия", "Москва", "146.2"],
        &["Česko", "Praha", "10.9"],
    ]);
    let table = render_table(
        &["country", "capital", "people (M)"],
        &body,
        &[Align::Left, Align::Center, Align::Right],
        8,
    );
    let widths: Vec<usize> = table.lines().map(display_width).collect();
    assert_eq!(widths.len(), 6);
    assert!(widths.iter().all(|&w| w == widths[0]), "{table}\n{widths:?}");
}

// Behaviour that already worked and must not change.

#[test]
fn ascii_behaviour_is_unchanged() {
    assert_eq!(
        wrap("the quick brown fox jumps over the lazy dog", 12),
        vec!["the quick", "brown fox", "jumps over", "the lazy dog"]
    );
    assert_eq!(wrap("go abcdefghij x", 4), vec!["go", "abcd", "efgh", "ij x"]);
    assert_eq!(wrap("abcdefgh", 4), vec!["abcd", "efgh"]);
    assert_eq!(wrap("abcdefgh z", 4), vec!["abcd", "efgh", "z"]);
    assert_eq!(wrap("ab abcdefgh", 4), vec!["ab", "abcd", "efgh"]);
    assert!(wrap("anything", 0).is_empty());
    assert!(wrap("", 5).is_empty());
    assert_eq!(truncate("hello world", 8), "hello w…");
    assert_eq!(truncate("hello", 5), "hello");
    assert_eq!(truncate("hello", 1), "…");
    assert_eq!(truncate("", 0), "");
    assert_eq!(pad("ab", 7, Align::Center), "  ab   ");
    assert_eq!(pad("abc", 3, Align::Right), "abc");
    assert_eq!(pad("", 2, Align::Left), "  ");
    let body = rows(&[&["apple", "3"], &["kiwi", "12"]]);
    assert_eq!(
        render_table(&["fruit", "qty"], &body, &[Align::Center, Align::Right], 10),
        "fruit | qty\n------+----\napple |   3\nkiwi  |  12"
    );
    assert_eq!(
        render_table(&["description"], &rows(&[&["short"]]), &[Align::Right], 6),
        "descr…\n------\n short"
    );
}

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

const ALPHABET: &[char] = &['a', 'b', 'z', 'é', 'ß', 'ж', 'Ω', '日', '語', '…', ' ', ' ', '\n'];

fn random_text(rng: &mut Lcg) -> String {
    let len = rng.below(40);
    (0..len).map(|_| ALPHABET[rng.below(ALPHABET.len())]).collect()
}

fn oracle_wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if width == 0 {
        return lines;
    }
    let mut cur: Vec<char> = Vec::new();
    for word in text.split_whitespace() {
        let w: Vec<char> = word.chars().collect();
        if w.len() > width {
            if !cur.is_empty() {
                lines.push(cur.drain(..).collect());
            }
            let mut i = 0;
            while w.len() - i > width {
                lines.push(w[i..i + width].iter().collect());
                i += width;
            }
            cur = w[i..].to_vec();
        } else if cur.is_empty() {
            cur = w;
        } else if cur.len() + 1 + w.len() <= width {
            cur.push(' ');
            cur.extend(w);
        } else {
            lines.push(cur.drain(..).collect());
            cur = w;
        }
    }
    if !cur.is_empty() {
        lines.push(cur.into_iter().collect());
    }
    lines
}

#[test]
fn random_text_matches_the_specification() {
    let mut rng = Lcg(7);
    for _ in 0..5000 {
        let text = random_text(&mut rng);
        let width = rng.below(12);
        assert_eq!(wrap(&text, width), oracle_wrap(&text, width), "wrap({text:?}, {width})");

        let n = display_width(&text);
        let expected = if n <= width {
            text.clone()
        } else if width == 0 {
            String::new()
        } else {
            text.chars().take(width - 1).chain(['…']).collect()
        };
        assert_eq!(truncate(&text, width), expected, "truncate({text:?}, {width})");

        for align in [Align::Left, Align::Right, Align::Center] {
            let padded = pad(&text, width, align);
            assert_eq!(display_width(&padded), n.max(width), "pad({text:?}, {width}, {align:?})");
            let extra = n.max(width) - n;
            let left = match align {
                Align::Left => 0,
                Align::Right => extra,
                Align::Center => extra / 2,
            };
            let expected: String = " ".repeat(left) + &text + &" ".repeat(extra - left);
            assert_eq!(padded, expected, "pad({text:?}, {width}, {align:?})");
        }
    }
}
