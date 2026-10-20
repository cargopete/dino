use crate::model::project::ProjectTotals;

/// One line of the summary: a project, or the total across them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRow {
    pub label: String,
    pub sessions: usize,
    pub minutes: u64,
    pub billable_minutes: u64,
    pub amount_cents: u64,
}

impl SummaryRow {
    #[must_use]
    pub fn from_totals(totals: &ProjectTotals) -> Self {
        Self {
            label: totals.name.clone(),
            sessions: totals.sessions,
            minutes: totals.minutes,
            billable_minutes: totals.billable_minutes,
            amount_cents: totals.amount_cents,
        }
    }

    /// Share of the time that was billable, in whole percent.
    #[must_use]
    pub fn billable_share(&self) -> u64 {
        crate::util::num::percent(self.billable_minutes, self.minutes)
    }
}
