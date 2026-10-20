#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// Pads `text` with spaces to `width` columns. Text already that wide or wider
/// is returned as it is.
#[must_use]
pub fn pad(text: &str, width: usize, align: Align) -> String {
    if text.len() >= width {
        return text.to_string();
    }
    let extra = width - text.len();
    let (left, right) = match align {
        Align::Left => (0, extra),
        Align::Right => (extra, 0),
        Align::Center => (extra / 2, extra - extra / 2),
    };
    format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
}
