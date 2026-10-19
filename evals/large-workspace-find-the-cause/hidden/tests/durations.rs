use fixture::App;

const LOG: &str = "\
2026-03-02 09:00-10:00 acme/design #billable Header layout
2026-03-03 14:00+30m acme/design #billable Footer
2026-03-02 11:00+45m globex Deploy window
2026-03-04 08:00-09:20 initech/api #billable Rate limits
2026-03-05 23:30-00:15 initech/api #billable Incident
";

fn config(extra: &str) -> String {
    format!(
        "[report]\n{extra}\n[billing]\ncurrency = EUR\ndefault_rate = 80.00\nrate.acme = 95.00\n"
    )
}

fn app(extra: &str) -> App {
    App::from_sources(&config(extra), LOG).unwrap()
}

fn words(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

fn table(text: &str) -> Vec<Vec<&str>> {
    text.lines().map(words).collect()
}

#[test]
fn the_summary_shows_hours_and_minutes() {
    let summary = app("").summary().unwrap();
    assert_eq!(
        table(&summary),
        vec![
            vec!["Summary", "2026-03-02", "to", "2026-03-05"],
            vec!["project", "sessions", "time", "billable", "amount"],
            vec!["acme", "2", "1h", "30m", "1h", "30m", "€142.50"],
            vec!["globex", "1", "45m", "0m", "€0.00"],
            vec!["initech", "2", "2h", "5m", "2h", "5m", "€166.67"],
            vec!["total", "5", "4h", "20m", "3h", "35m", "€309.17"],
        ],
        "{summary}"
    );
}

#[test]
fn whole_hours_have_no_minutes() {
    let app = App::from_sources(&config(""), "2026-03-02 09:00-11:00 acme #billable\n").unwrap();
    let summary = app.summary().unwrap();
    assert_eq!(words(summary.lines().nth(2).unwrap()), ["acme", "1", "2h", "2h", "€190.00"]);
}

#[test]
fn the_clock_style_carries_hours() {
    let summary = app("duration_style = clock").summary().unwrap();
    let rows = table(&summary);
    assert_eq!(rows[2], ["acme", "2", "1:30", "1:30", "€142.50"]);
    assert_eq!(rows[3], ["globex", "1", "0:45", "0:00", "€0.00"]);
    assert_eq!(rows[4], ["initech", "2", "2:05", "2:05", "€166.67"]);
    assert_eq!(rows[5], ["total", "5", "4:20", "3:35", "€309.17"]);
}

#[test]
fn the_decimal_style_is_unchanged() {
    let summary = app("duration_style = decimal").summary().unwrap();
    let rows = table(&summary);
    assert_eq!(rows[2], ["acme", "2", "1.50h", "1.50h", "€142.50"]);
    assert_eq!(rows[3], ["globex", "1", "0.75h", "0.00h", "€0.00"]);
    assert_eq!(rows[4], ["initech", "2", "2.08h", "2.08h", "€166.67"]);
}

#[test]
fn rounding_happens_before_formatting() {
    let app = App::from_sources(
        &config("round_to = 15"),
        "2026-03-02 08:00-09:37 acme #billable\n2026-03-02 10:00-10:07 globex\n",
    )
    .unwrap();
    let summary = app.summary().unwrap();
    let rows = table(&summary);
    assert_eq!(rows[2], ["acme", "1", "1h", "30m", "1h", "30m", "€142.50"]);
    assert_eq!(rows[3], ["globex", "1", "0m", "0m", "€0.00"]);
}

#[test]
fn sorting_and_the_week_filter_still_apply() {
    let log = format!("{LOG}2026-03-09 09:00-12:00 hooli #billable Next week\n");
    let app = App::from_sources(&config("sort = duration\nshow_total = false\n[filter]\nweek = 2026-W10"), &log)
        .unwrap();
    let summary = app.summary().unwrap();
    let rows = table(&summary);
    assert_eq!(rows[0], ["Summary", "2026-03-02", "to", "2026-03-08"]);
    assert_eq!(rows[2][0], "initech");
    assert_eq!(rows[3][0], "acme");
    assert_eq!(rows[4][0], "globex");
    assert_eq!(rows.len(), 5);
}

#[test]
fn the_csv_export_formats_long_durations() {
    let csv = app("").export("csv").unwrap();
    assert_eq!(
        csv,
        "project,sessions,minutes,time,billable_minutes,amount,currency\n\
         acme,2,90,1h 30m,90,142.50,EUR\n\
         globex,1,45,45m,0,0.00,EUR\n\
         initech,2,125,2h 5m,125,166.67,EUR\n\
         total,5,260,4h 20m,215,309.17,EUR\n"
    );
}

#[test]
fn the_markdown_export_formats_long_durations() {
    let markdown = app("").export("markdown").unwrap();
    assert!(markdown.contains("| acme | 1h 30m | 1h 30m | €142.50 |\n"), "{markdown}");
    assert!(markdown.contains("| **Total** | **4h 20m** | **3h 35m** | **€309.17** |\n"), "{markdown}");
}

#[test]
fn the_json_export_splits_hours_from_minutes() {
    let json = app("").export("json").unwrap();
    assert!(
        json.contains(r#"{"name":"initech","sessions":2,"minutes":125,"time":{"hours":2,"minutes":5},"display":"2h 5m","billable_minutes":125,"amount_cents":16667}"#),
        "{json}"
    );
    assert!(json.contains(r#""total":{"name":"total","sessions":5,"minutes":260,"time":{"hours":4,"minutes":20}"#), "{json}");
}

#[test]
fn the_detail_report_is_unchanged() {
    let detail = app("").detail("initech").unwrap();
    assert_eq!(
        detail,
        "initech: 2 sessions, 2 hours 5 minutes\n\
         2026-03-04  08:00  1h 20m  01:20:00  api  Rate limits\n\
         2026-03-05  23:30  45m  00:45:00  api  Incident\n"
    );
}

#[test]
fn the_clock_helpers_split_correctly() {
    use fixture::util::clock::{split_hm, split_hms, time_of_day};
    assert_eq!(split_hm(0), (0, 0));
    assert_eq!(split_hm(59), (0, 59));
    assert_eq!(split_hm(60), (1, 0));
    assert_eq!(split_hm(90), (1, 30));
    assert_eq!(split_hm(1500), (25, 0));
    assert_eq!(split_hms(5400), (1, 30, 0));
    assert_eq!(split_hms(3725), (1, 2, 5));
    assert_eq!(split_hms(59), (0, 0, 59));
    assert_eq!(time_of_day(1530), "01:30");
    assert_eq!(time_of_day(545), "09:05");
}

#[test]
fn the_other_duration_formatters_are_untouched() {
    use fixture::fmt::duration::format_duration_long;
    use fixture::legacy::fmt_v1;
    use fixture::util::time::{format_duration, Seconds};
    assert_eq!(format_duration(5400), "1h 30m");
    assert_eq!(format_duration(7200), "2h");
    assert_eq!(Seconds(5400).stopwatch(), "01:30:00");
    assert_eq!(format_duration_long(61), "1 hour 1 minute");
    assert_eq!(format_duration_long(0), "0 minutes");
    assert_eq!(fmt_v1::format_duration(90), "30m");
    assert_eq!(fmt_v1::format_hours(90), "1h");
}
