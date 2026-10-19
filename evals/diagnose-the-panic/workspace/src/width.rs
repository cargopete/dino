/// The display width of `text`: one column per `char`.
#[must_use]
pub fn display_width(text: &str) -> usize {
    text.chars().count()
}
