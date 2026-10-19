use std::collections::BTreeMap;

use crate::model::entry::Entry;
use crate::model::period::Period;
use crate::util::iter::group_by;

/// Every entry in the log, in the order they were written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    entries: Vec<Entry>,
}

impl Ledger {
    #[must_use]
    pub fn new(entries: Vec<Entry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    #[must_use]
    pub fn within(self, period: Period) -> Self {
        Self {
            entries: self
                .entries
                .into_iter()
                .filter(|entry| period.contains(entry.date))
                .collect(),
        }
    }

    #[must_use]
    pub fn for_project(&self, project: &str) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.project == project)
            .collect()
    }

    #[must_use]
    pub fn by_project(&self) -> BTreeMap<String, Vec<&Entry>> {
        group_by(self.entries.iter(), |entry| entry.project.clone())
    }

    /// The first and last dates in the ledger.
    #[must_use]
    pub fn span(&self) -> Option<Period> {
        let first = self.entries.iter().map(|entry| entry.date).min()?;
        let last = self.entries.iter().map(|entry| entry.date).max()?;
        Period::new(first, last)
    }

    #[must_use]
    pub fn total_minutes(&self) -> u64 {
        self.entries.iter().map(|entry| u64::from(entry.minutes)).sum()
    }
}
