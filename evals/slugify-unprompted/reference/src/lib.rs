//! Helpers for the blog's URLs.

/// The year and month a post's path starts with, as `2026/10`.
pub fn date_prefix(year: u32, month: u32) -> String {
    format!("{year}/{month:02}")
}

/// A post title as a URL slug.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(c.to_ascii_lowercase());
        } else if c == '\'' {
            // An apostrophe joins: "don't" becomes "dont".
        } else {
            gap = true;
        }
    }
    slug.truncate(slug.len().min(60));
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn words_are_joined_by_single_hyphens() {
        assert_eq!(slugify("  Hello,   World!  "), "hello-world");
    }
}
