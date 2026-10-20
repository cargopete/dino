use crate::fmt::duration::format_duration_long;
use crate::model::entry::Entry;
use crate::model::ledger::Ledger;
use crate::util::clock::time_of_day;
use crate::util::time::{format_duration, Seconds};

/// Every session for one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub project: String,
    pub entries: Vec<Entry>,
}

impl Detail {
    #[must_use]
    pub fn build(ledger: &Ledger, project: &str) -> Option<Self> {
        let entries: Vec<Entry> = ledger.for_project(project).into_iter().cloned().collect();
        (!entries.is_empty()).then(|| Self {
            project: project.to_owned(),
            entries,
        })
    }

    #[must_use]
    pub fn total_minutes(&self) -> u64 {
        self.entries.iter().map(|entry| u64::from(entry.minutes)).sum()
    }

    /// A heading line, then one line per session:
    /// date, start, length, stopwatch length, task and note.
    #[must_use]
    pub fn render(&self) -> String {
        let sessions = match self.entries.len() {
            1 => "1 session".to_owned(),
            n => format!("{n} sessions"),
        };
        let mut out = format!(
            "{}: {}, {}\n",
            self.project,
            sessions,
            format_duration_long(self.total_minutes())
        );
        for entry in &self.entries {
            let length = Seconds::from_minutes(u64::from(entry.minutes));
            let mut line = format!(
                "{}  {}  {}  {}",
                entry.date,
                time_of_day(entry.start),
                format_duration(length.0),
                length.stopwatch(),
            );
            if let Some(task) = &entry.task {
                line.push_str("  ");
                line.push_str(task);
            }
            if !entry.note.is_empty() {
                line.push_str("  ");
                line.push_str(&entry.note);
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}
