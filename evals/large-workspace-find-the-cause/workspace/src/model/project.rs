use crate::model::entry::Entry;
use crate::model::rate::Rate;
use crate::util::round::round_to_nearest;

/// What one project adds up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTotals {
    pub name: String,
    pub minutes: u64,
    pub billable_minutes: u64,
    pub sessions: usize,
    pub amount_cents: u64,
}

impl ProjectTotals {
    #[must_use]
    pub fn from_entries(name: &str, entries: &[&Entry], rate: Rate, round_to: u64) -> Self {
        let minutes: u64 = entries.iter().map(|entry| u64::from(entry.minutes)).sum();
        let billable: u64 = entries
            .iter()
            .filter(|entry| entry.billable)
            .map(|entry| u64::from(entry.minutes))
            .sum();
        let billable_minutes = round_to_nearest(billable, round_to);
        Self {
            name: name.to_owned(),
            minutes: round_to_nearest(minutes, round_to),
            billable_minutes,
            sessions: entries.len(),
            amount_cents: rate.charge(billable_minutes),
        }
    }
}
