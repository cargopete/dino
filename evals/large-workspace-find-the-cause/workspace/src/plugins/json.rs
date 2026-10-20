use crate::plugins::{RenderContext, ReportPlugin};
use crate::report::row::SummaryRow;
use crate::report::summary::Summary;
use crate::util::clock::split_hm;
use crate::util::escape::json_string;

/// A single JSON object, written by hand to keep the crate dependency-free.
/// Time is given both as raw minutes and split into hours and minutes.
pub struct JsonPlugin;

impl ReportPlugin for JsonPlugin {
    fn name(&self) -> &'static str {
        "json"
    }

    fn render(&self, summary: &Summary, context: &RenderContext) -> String {
        let period = match summary.period {
            Some(period) => format!(
                "{{\"start\":\"{}\",\"end\":\"{}\"}}",
                period.start, period.end
            ),
            None => "null".to_owned(),
        };
        let projects: Vec<String> = summary.rows.iter().map(|row| object(row, context)).collect();
        let total = match &summary.total {
            Some(total) => object(total, context),
            None => "null".to_owned(),
        };
        format!(
            "{{\"period\":{period},\"currency\":{},\"projects\":[{}],\"total\":{total}}}\n",
            json_string(context.currency.code()),
            projects.join(",")
        )
    }
}

fn object(row: &SummaryRow, context: &RenderContext) -> String {
    let (hours, minutes) = split_hm(row.minutes);
    format!(
        "{{\"name\":{},\"sessions\":{},\"minutes\":{},\"time\":{{\"hours\":{hours},\"minutes\":{minutes}}},\"display\":{},\"billable_minutes\":{},\"amount_cents\":{}}}",
        json_string(&row.label),
        row.sessions,
        row.minutes,
        json_string(&context.duration(row.minutes)),
        row.billable_minutes,
        row.amount_cents,
    )
}
