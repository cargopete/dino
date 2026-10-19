use crate::config::Config;
use crate::model::ledger::Ledger;
use crate::model::period::Period;
use crate::report::row::SummaryRow;
use crate::report::sort::sort_rows;
use crate::report::totals::{grand_total, project_totals};

/// Everything a summary plugin needs, already added up and sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub period: Option<Period>,
    pub rows: Vec<SummaryRow>,
    pub total: Option<SummaryRow>,
}

impl Summary {
    #[must_use]
    pub fn build(ledger: &Ledger, config: &Config) -> Self {
        let mut rows: Vec<SummaryRow> = project_totals(ledger, config)
            .iter()
            .map(SummaryRow::from_totals)
            .collect();
        sort_rows(&mut rows, config.report.sort);
        let total = (config.report.show_total && !rows.is_empty()).then(|| grand_total(&rows));
        Self {
            period: config.filter.period.or_else(|| ledger.span()),
            rows,
            total,
        }
    }

    /// The project rows followed by the total, when there is one.
    pub fn lines(&self) -> impl Iterator<Item = &SummaryRow> {
        self.rows.iter().chain(self.total.as_ref())
    }
}
