use crate::display_width;

/// Wraps `text` into lines of at most `width` columns.
#[must_use]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if width == 0 {
        return lines;
    }
    let mut line = String::new();
    let mut line_width = 0;
    for word in text.split_whitespace() {
        let word_width = display_width(word);
        if word_width > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let chars: Vec<char> = word.chars().collect();
            let mut chunks = chars.chunks(width).map(|c| c.iter().collect::<String>());
            let mut last = chunks.next().unwrap_or_default();
            for chunk in chunks {
                lines.push(std::mem::replace(&mut last, chunk));
            }
            line_width = display_width(&last);
            line = last;
        } else if line.is_empty() {
            line.push_str(word);
            line_width = word_width;
        } else if line_width + 1 + word_width <= width {
            line.push(' ');
            line.push_str(word);
            line_width += 1 + word_width;
        } else {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
            line_width = word_width;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
