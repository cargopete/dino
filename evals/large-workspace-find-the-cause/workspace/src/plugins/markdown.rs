use crate::fmt::date_fmt::range;
use crate::plugins::{RenderContext, ReportPlugin};
use crate::report::summary::Summary;
use crate::util::escape::markdown_cell;

pub struct MarkdownPlugin;

impl ReportPlugin for MarkdownPlugin {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn render(&self, summary: &Summary, context: &RenderContext) -> String {
        let mut out = match summary.period {
            Some(period) => format!("## Summary {}\n\n", range(period)),
            None => return "## Summary\n\nNo entries.\n".to_owned(),
        };
        out.push_str("| Project | Time | Billable | Amount |\n");
        out.push_str("|:--|--:|--:|--:|\n");
        for row in &summary.rows {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                markdown_cell(&row.label),
                context.duration(row.minutes),
                context.duration(row.billable_minutes),
                context.money(row.amount_cents),
            ));
        }
        if let Some(total) = &summary.total {
            out.push_str(&format!(
                "| **Total** | **{}** | **{}** | **{}** |\n",
                context.duration(total.minutes),
                context.duration(total.billable_minutes),
                context.money(total.amount_cents),
            ));
        }
        out
    }
}
