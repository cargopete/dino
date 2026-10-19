use crate::fmt::money::plain_amount;
use crate::plugins::{RenderContext, ReportPlugin};
use crate::report::summary::Summary;
use crate::util::escape::csv_field;

/// One row per project. Minutes are given raw beside the formatted time so a
/// spreadsheet can add them up.
pub struct CsvPlugin;

impl ReportPlugin for CsvPlugin {
    fn name(&self) -> &'static str {
        "csv"
    }

    fn render(&self, summary: &Summary, context: &RenderContext) -> String {
        let mut out = String::from("project,sessions,minutes,time,billable_minutes,amount,currency\n");
        for row in summary.lines() {
            let fields = [
                csv_field(&row.label),
                row.sessions.to_string(),
                row.minutes.to_string(),
                csv_field(&context.duration(row.minutes)),
                row.billable_minutes.to_string(),
                plain_amount(row.amount_cents, false),
                context.currency.code().to_owned(),
            ];
            out.push_str(&fields.join(","));
            out.push('\n');
        }
        out
    }
}
