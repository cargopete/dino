use crate::config::Config;
use crate::model::ledger::Ledger;
use crate::model::project::ProjectTotals;
use crate::report::row::SummaryRow;

pub fn project_totals(ledger: &Ledger, config: &Config) -> Vec<ProjectTotals> {
    ledger
        .by_project()
        .iter()
        .map(|(name, entries)| {
            ProjectTotals::from_entries(
                name,
                entries,
                config.rate_for(name),
                config.report.round_to,
            )
        })
        .collect()
}

/// The sum of the rows. It adds the already-rounded project figures, so the
/// total always agrees with the lines above it.
#[must_use]
pub fn grand_total(rows: &[SummaryRow]) -> SummaryRow {
    SummaryRow {
        label: "total".to_owned(),
        sessions: rows.iter().map(|row| row.sessions).sum(),
        minutes: rows.iter().map(|row| row.minutes).sum(),
        billable_minutes: rows.iter().map(|row| row.billable_minutes).sum(),
        amount_cents: rows.iter().map(|row| row.amount_cents).sum(),
    }
}
