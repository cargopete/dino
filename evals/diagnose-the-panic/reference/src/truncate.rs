const ELLIPSIS: char = '…';

/// Shortens `text` to at most `max` columns, ending in an ellipsis when
/// anything was cut.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if crate::display_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push(ELLIPSIS);
    out
}
