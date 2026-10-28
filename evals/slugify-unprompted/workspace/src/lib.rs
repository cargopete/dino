//! Helpers for the blog's URLs.

/// The year and month a post's path starts with, as `2026/10`.
pub fn date_prefix(year: u32, month: u32) -> String {
    format!("{year}/{month:02}")
}
