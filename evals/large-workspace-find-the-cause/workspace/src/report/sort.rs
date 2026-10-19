use std::cmp::Reverse;

use crate::config::SortKey;
use crate::report::row::SummaryRow;

pub fn sort_rows(rows: &mut [SummaryRow], key: SortKey) {
    match key {
        SortKey::Name => rows.sort_by(|a, b| a.label.cmp(&b.label)),
        SortKey::Duration => {
            rows.sort_by(|a, b| (Reverse(a.minutes), &a.label).cmp(&(Reverse(b.minutes), &b.label)));
        }
    }
}
