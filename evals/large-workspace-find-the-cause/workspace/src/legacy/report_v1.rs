use crate::legacy::fmt_v1::{format_amount, format_duration, format_hours};
use crate::model::ledger::Ledger;
use crate::model::rate::Rate;

/// The 0.x summary: one line per project with hours and minutes in
/// separate columns.
#[must_use]
pub fn summary_v1(ledger: &Ledger, rate: Rate) -> String {
    let mut out = String::from("PROJECT HOURS MINUTES AMOUNT\n");
    for (project, entries) in ledger.by_project() {
        let minutes: u32 = entries.iter().map(|entry| entry.minutes).sum();
        let billable: u64 = entries
            .iter()
            .filter(|entry| entry.billable)
            .map(|entry| u64::from(entry.minutes))
            .sum();
        out.push_str(&format!(
            "{} {} {} {}\n",
            project,
            format_hours(minutes),
            format_duration(minutes),
            format_amount(rate.charge(billable)),
        ));
    }
    out
}
