use crate::fmt::date_fmt::range;
use crate::fmt::table::{Align, Table};
use crate::plugins::{RenderContext, ReportPlugin};
use crate::report::summary::Summary;

/// The terminal table.
pub struct PlainPlugin;

impl ReportPlugin for PlainPlugin {
    fn name(&self) -> &'static str {
        "plain"
    }

    fn render(&self, summary: &Summary, context: &RenderContext) -> String {
        let heading = match summary.period {
            Some(period) => format!("Summary {}\n", range(period)),
            None => return "Summary (no entries)\n".to_owned(),
        };
        let mut table = Table::new(
            &["project", "sessions", "time", "billable", "amount"],
            &[Align::Left, Align::Right, Align::Right, Align::Right, Align::Right],
        );
        for row in summary.lines() {
            table.push(vec![
                row.label.clone(),
                row.sessions.to_string(),
                context.duration(row.minutes),
                context.duration(row.billable_minutes),
                context.money(row.amount_cents),
            ]);
        }
        heading + &table.render()
    }
}
