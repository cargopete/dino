use fixture::{Buffer, EditError};

fn typed(text: &str) -> Buffer {
    let mut b = Buffer::new();
    for (i, c) in text.chars().enumerate() {
        b.insert(i, &c.to_string()).unwrap();
    }
    b
}

#[test]
fn typing_a_word_undoes_at_once() {
    let mut b = typed("hello");
    assert!(b.undo());
    assert_eq!(b.text(), "");
    assert!(!b.undo());
}

#[test]
fn a_newline_ends_the_run_and_is_its_own_step() {
    let mut b = typed("ab\ncd");
    assert!(b.undo());
    assert_eq!(b.text(), "ab\n");
    assert!(b.undo());
    assert_eq!(b.text(), "ab");
    assert!(b.undo());
    assert_eq!(b.text(), "");
}

#[test]
fn typing_elsewhere_starts_a_new_step() {
    let mut b = typed("abc");
    b.insert(0, "x").unwrap();
    b.insert(1, "y").unwrap();
    assert_eq!(b.text(), "xyabc");
    b.undo();
    assert_eq!(b.text(), "abc");
}

#[test]
fn pasting_is_one_step_and_never_joins() {
    let mut b = typed("ab");
    b.insert(2, "cd").unwrap();
    b.insert(4, "e").unwrap();
    b.undo();
    assert_eq!(b.text(), "abcd");
    b.undo();
    assert_eq!(b.text(), "ab");
}

#[test]
fn backspacing_joins_and_forward_deleting_joins() {
    let mut b = Buffer::from_text("abcdef");
    b.delete(5..6).unwrap();
    b.delete(4..5).unwrap();
    b.delete(3..4).unwrap();
    assert_eq!(b.text(), "abc");
    b.undo();
    assert_eq!(b.text(), "abcdef");
    b.delete(1..2).unwrap();
    b.delete(1..2).unwrap();
    assert_eq!(b.text(), "adef");
    b.undo();
    assert_eq!(b.text(), "abcdef");
}

#[test]
fn a_delete_run_keeps_its_direction() {
    let mut b = Buffer::from_text("abcdef");
    b.delete(3..4).unwrap(); // abcef
    b.delete(2..3).unwrap(); // backspace: abef
    b.delete(2..3).unwrap(); // forward from the new start: a new step
    assert_eq!(b.text(), "abf");
    b.undo();
    assert_eq!(b.text(), "abef");
}

#[test]
fn typing_then_backspace_are_separate() {
    let mut b = typed("abc");
    b.delete(2..3).unwrap();
    b.undo();
    assert_eq!(b.text(), "abc");
    b.undo();
    assert_eq!(b.text(), "");
}

#[test]
fn undo_breaks_the_run() {
    let mut b = typed("ab");
    b.undo();
    b.redo();
    b.insert(2, "c").unwrap();
    b.undo();
    assert_eq!(b.text(), "ab");
}

#[test]
fn a_new_edit_clears_redo() {
    let mut b = typed("ab");
    b.undo();
    assert!(b.can_redo());
    b.insert(0, "z").unwrap();
    assert!(!b.can_redo());
    assert!(!b.redo());
}

#[test]
fn redo_after_several_undos() {
    let mut b = typed("ab");
    b.insert(2, "\n").unwrap();
    b.insert(3, "cd").unwrap();
    while b.undo() {}
    assert_eq!(b.text(), "");
    assert!(b.redo());
    assert_eq!(b.text(), "ab");
    assert!(b.redo());
    assert_eq!(b.text(), "ab\n");
    assert!(b.redo());
    assert_eq!(b.text(), "ab\ncd");
    assert!(!b.redo());
}

#[test]
fn groups_undo_as_one_and_nest() {
    let mut b = Buffer::from_text("x");
    b.begin_group();
    b.insert(1, "a").unwrap();
    b.begin_group();
    b.delete(0..1).unwrap();
    b.end_group();
    b.insert(1, "\nb").unwrap();
    b.end_group();
    assert_eq!(b.text(), "a\nb");
    b.undo();
    assert_eq!(b.text(), "x");
    b.redo();
    assert_eq!(b.text(), "a\nb");
}

#[test]
fn an_empty_group_records_nothing() {
    let mut b = typed("ab");
    b.begin_group();
    b.end_group();
    b.undo();
    assert_eq!(b.text(), "");
}

#[test]
fn a_group_breaks_typing_on_both_sides() {
    let mut b = typed("ab");
    b.begin_group();
    b.insert(2, "c").unwrap();
    b.end_group();
    b.insert(3, "d").unwrap();
    b.undo();
    assert_eq!(b.text(), "abc");
    b.undo();
    assert_eq!(b.text(), "ab");
}

#[test]
fn undo_closes_an_open_group() {
    let mut b = Buffer::new();
    b.begin_group();
    b.insert(0, "a").unwrap();
    b.insert(1, "b").unwrap();
    assert!(b.undo());
    assert_eq!(b.text(), "");
    b.insert(0, "c").unwrap();
    b.insert(1, "d").unwrap();
    b.undo();
    assert_eq!(b.text(), "", "typing after the group was closed by undo");
    b.end_group();
    assert!(!b.undo(), "a stray end changes nothing");
}

#[test]
fn errors_and_empty_edits_change_nothing() {
    let mut b = typed("ab");
    assert_eq!(b.insert(5, "x"), Err(EditError::OutOfRange));
    assert_eq!(b.delete(1..9), Err(EditError::OutOfRange));
    assert_eq!(b.delete(2..1), Err(EditError::OutOfRange));
    assert_eq!(b.insert(2, ""), Ok(()));
    assert_eq!(b.delete(1..1), Ok(String::new()));
    b.insert(2, "c").unwrap();
    b.undo();
    assert_eq!(b.text(), "", "the run survived the failed and empty edits");
}

#[test]
fn positions_count_characters_not_bytes() {
    let mut b = Buffer::from_text("é🦀\n中x");
    assert_eq!(b.len_chars(), 5);
    assert_eq!(b.line_col(3), Ok((1, 0)));
    assert_eq!(b.line_col(5), Ok((1, 2)));
    assert_eq!(b.line_col(6), Err(EditError::OutOfRange));
    assert_eq!(b.offset(1, 1), Ok(4));
    assert_eq!(b.offset(0, 3), Err(EditError::OutOfRange));
    assert_eq!(b.offset(2, 0), Err(EditError::OutOfRange));
    assert_eq!(b.delete(1..2).unwrap(), "🦀");
    b.insert(1, "\u{301}").unwrap();
    assert_eq!(b.text(), "é\u{301}\n中x");
}

#[test]
fn lines() {
    let b = Buffer::from_text("a\n\nb\n");
    assert_eq!(b.len_lines(), 4);
    assert_eq!(b.line(0).as_deref(), Some("a"));
    assert_eq!(b.line(1).as_deref(), Some(""));
    assert_eq!(b.line(3).as_deref(), Some(""));
    assert_eq!(b.line(4), None);
    assert_eq!(Buffer::new().len_lines(), 1);
    assert_eq!(Buffer::new().offset(0, 0), Ok(0));
}
