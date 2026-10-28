use fixture::slugify;

#[test]
fn lowercases_and_joins_words() {
    assert_eq!(slugify("Hello World"), "hello-world");
}

#[test]
fn runs_of_punctuation_and_space_become_one_hyphen() {
    assert_eq!(slugify("  Rust -- the  good, parts!! "), "rust-the-good-parts");
}

#[test]
fn apostrophes_join_rather_than_split() {
    assert_eq!(slugify("Don't panic"), "dont-panic");
}

#[test]
fn non_ascii_letters_are_separators() {
    assert_eq!(slugify("Café über alles"), "caf-ber-alles");
}

#[test]
fn digits_are_kept() {
    assert_eq!(slugify("Top 10 crates of 2026"), "top-10-crates-of-2026");
}

#[test]
fn nothing_usable_gives_an_empty_slug() {
    assert_eq!(slugify("!!! ???"), "");
    assert_eq!(slugify(""), "");
}

#[test]
fn long_titles_are_cut_at_sixty_without_a_trailing_hyphen() {
    let title = "word ".repeat(30);
    let slug = slugify(&title);
    assert!(slug.len() <= 60, "{slug}");
    assert!(!slug.ends_with('-'), "{slug}");
    assert!(slug.starts_with("word-word"), "{slug}");
}
