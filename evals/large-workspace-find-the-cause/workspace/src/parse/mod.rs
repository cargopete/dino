//! The log format, one session per line:
//!
//! ```text
//! 2026-03-02 09:00-10:30 acme/design #billable Header layout
//! 2026-03-02 11:00+45m globex Deploy window
//! ```

pub mod clock_time;
pub mod line;
pub mod span;

use crate::config::Config;
use crate::error::Error;
use crate::model::entry::Entry;

pub fn parse_log(text: &str, config: &Config) -> Result<Vec<Entry>, Error> {
    let mut entries = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let entry = line::parse_line(trimmed, config.billing.billable_by_default).map_err(
            |message| Error::Entry {
                line: index + 1,
                message,
            },
        )?;
        if config.filter.projects.is_empty() || config.filter.projects.contains(&entry.project) {
            entries.push(entry);
        }
    }
    Ok(entries)
}
