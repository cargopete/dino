use crate::model::date::Date;
use crate::model::tag::{self, Tag};

/// One session from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub date: Date,
    /// Minutes after midnight the session began.
    pub start: u32,
    pub minutes: u32,
    pub project: String,
    pub task: Option<String>,
    pub tags: Vec<Tag>,
    pub note: String,
    pub billable: bool,
}

impl Entry {
    /// Whether the tags settle billing, falling back to the given default.
    #[must_use]
    pub fn billable_by_tags(tags: &[Tag], default: bool) -> bool {
        if tags.iter().any(|tag| tag.is(tag::NON_BILLABLE)) {
            false
        } else if tags.iter().any(|tag| tag.is(tag::BILLABLE)) {
            true
        } else {
            default
        }
    }

    /// Minutes after midnight the session ended, which may run past 1440.
    #[must_use]
    pub fn end(&self) -> u32 {
        self.start + self.minutes
    }

    #[must_use]
    pub fn has_tag(&self, name: &str) -> bool {
        self.tags.iter().any(|tag| tag.is(name))
    }
}
