/// Display width in characters. Good enough for the names in a time log,
/// which do not use wide or combining characters.
#[must_use]
pub fn width(text: &str) -> usize {
    text.chars().count()
}

#[must_use]
pub fn pad_right(text: &str, to: usize) -> String {
    let mut padded = text.to_owned();
    padded.push_str(&" ".repeat(to.saturating_sub(width(text))));
    padded
}

#[must_use]
pub fn pad_left(text: &str, to: usize) -> String {
    let mut padded = " ".repeat(to.saturating_sub(width(text)));
    padded.push_str(text);
    padded
}

/// Cuts text to at most `to` characters, ending in `…` when anything was cut.
#[must_use]
pub fn truncate(text: &str, to: usize) -> String {
    if width(text) <= to {
        return text.to_owned();
    }
    if to == 0 {
        return String::new();
    }
    let mut cut: String = text.chars().take(to - 1).collect();
    cut.push('…');
    cut
}
