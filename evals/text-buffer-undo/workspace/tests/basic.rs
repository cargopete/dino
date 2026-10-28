use fixture::{Buffer, EditError};

#[test]
fn edits_and_positions() {
    let mut b = Buffer::from_text("héllo\nwörld");
    assert_eq!(b.len_chars(), 11);
    assert_eq!(b.len_lines(), 2);
    assert_eq!(b.line(1).as_deref(), Some("wörld"));
    assert_eq!(b.line_col(7), Ok((1, 1)));
    assert_eq!(b.offset(1, 1), Ok(7));
    b.insert(5, ",").unwrap();
    assert_eq!(b.delete(0..1).unwrap(), "h");
    assert_eq!(b.text(), "éllo,\nwörld");
    assert_eq!(b.insert(99, "x"), Err(EditError::OutOfRange));
}

#[test]
fn typing_undoes_as_one_step() {
    let mut b = Buffer::new();
    for (i, c) in "abc".chars().enumerate() {
        b.insert(i, &c.to_string()).unwrap();
    }
    assert!(b.undo());
    assert_eq!(b.text(), "");
    assert!(b.redo());
    assert_eq!(b.text(), "abc");
}

#[test]
fn groups_undo_as_one_step() {
    let mut b = Buffer::from_text("x");
    b.begin_group();
    b.delete(0..1).unwrap();
    b.insert(0, "yz").unwrap();
    b.end_group();
    b.undo();
    assert_eq!(b.text(), "x");
}
