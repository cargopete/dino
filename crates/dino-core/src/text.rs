//! Shortening text for a tool result.

/// Of a capped process output, the share kept from the start. The rest of the
/// budget goes to the end, where a test run's failures and summary are.
const HEAD_SHARE: usize = 4;

/// Shortens `value` to about `max_bytes`, keeping its beginning.
#[must_use]
pub fn capped_head(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let end = floor_boundary(value, max_bytes);
    format!("{}\n… (truncated)", &value[..end])
}

/// Shortens process output to about `max_bytes`, keeping a quarter from the
/// beginning and the rest from the end, and saying how much was left out.
#[must_use]
pub fn capped_output(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let head_end = floor_boundary(value, max_bytes / HEAD_SHARE);
    let tail_start = ceil_boundary(value, value.len() - (max_bytes - max_bytes / HEAD_SHARE));
    let head = &value[..head_end];
    let tail = &value[tail_start..];
    let omitted = tail_start - head_end;
    format!("{head}\n… ({omitted} bytes omitted) …\n{tail}")
}

fn floor_boundary(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(value: &str, mut index: usize) -> usize {
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::capped_head;
    use super::capped_output;

    #[test]
    fn short_text_is_unchanged() {
        assert_eq!(capped_output("abc", 10), "abc");
        assert_eq!(capped_head("abc", 10), "abc");
    }

    #[test]
    fn output_keeps_both_ends() {
        let text = format!("start\n{}\ntest result: FAILED", "x".repeat(10_000));
        let capped = capped_output(&text, 400);
        assert!(capped.starts_with("start\n"));
        assert!(capped.ends_with("test result: FAILED"));
        assert!(capped.contains("bytes omitted"));
        assert!(capped.len() < 500);
    }

    #[test]
    fn output_respects_char_boundaries() {
        let text = "é".repeat(1_000);
        let capped = capped_output(&text, 101);
        assert!(capped.contains("bytes omitted"));
    }

    #[test]
    fn head_keeps_the_beginning() {
        let capped = capped_head(&"a".repeat(100), 10);
        assert_eq!(capped, format!("{}\n… (truncated)", "a".repeat(10)));
    }
}
