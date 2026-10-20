use fixture::{App, Error};

const CONFIG: &str = "\
[report]
duration_style = compact

[billing]
currency = EUR
default_rate = 80.00
rate.acme = 95.00
";

const LOG: &str = "\
# week ten
2026-03-02 09:00-09:40 acme/design #billable Header layout
2026-03-02 11:00+45m globex Deploy window
2026-03-03 14:00+15m acme/design #billable Footer
";

fn words(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

#[test]
fn the_summary_lists_each_project() {
    let app = App::from_sources(CONFIG, LOG).unwrap();
    let summary = app.summary().unwrap();
    let lines: Vec<&str> = summary.lines().collect();
    assert_eq!(lines[0], "Summary 2026-03-02 to 2026-03-03");
    assert_eq!(words(lines[1]), ["project", "sessions", "time", "billable", "amount"]);
    assert_eq!(words(lines[2]), ["acme", "2", "55m", "55m", "€87.08"]);
    assert_eq!(words(lines[3]), ["globex", "1", "45m", "0m", "€0.00"]);
}

#[test]
fn the_csv_export_keeps_raw_minutes() {
    let app = App::from_sources(CONFIG, LOG).unwrap();
    let csv = app.export("csv").unwrap();
    assert!(csv.contains("acme,2,55,55m,55,87.08,EUR\n"), "{csv}");
}

#[test]
fn the_detail_report_lists_sessions() {
    let app = App::from_sources(CONFIG, LOG).unwrap();
    let detail = app.detail("acme").unwrap();
    assert_eq!(
        detail,
        "acme: 2 sessions, 55 minutes\n\
         2026-03-02  09:00  40m  00:40:00  design  Header layout\n\
         2026-03-03  14:00  15m  00:15:00  design  Footer\n"
    );
}

#[test]
fn an_unknown_plugin_is_an_error() {
    let app = App::from_sources(CONFIG, LOG).unwrap();
    assert_eq!(app.export("xml"), Err(Error::UnknownPlugin("xml".to_owned())));
}

#[test]
fn a_bad_log_line_names_its_line() {
    let error = App::from_sources(CONFIG, "2026-03-02 09:00 acme\n").err().unwrap();
    assert!(matches!(error, Error::Entry { line: 1, .. }), "{error:?}");
}

#[test]
fn an_unknown_config_key_is_refused() {
    let error = App::from_sources("[report]\ncolour = red\n", "").err().unwrap();
    assert!(matches!(error, Error::Config { line: 2, .. }), "{error:?}");
}
