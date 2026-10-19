use crate::model::date::Date;
use crate::model::entry::Entry;
use crate::model::tag::Tag;
use crate::parse::span;
use crate::util::text::normalise_project;

/// Reads one log line: date, span, `project[/task]`, then any `#tags` and a
/// free-text note.
pub fn parse_line(line: &str, billable_by_default: bool) -> Result<Entry, String> {
    let mut words = line.split_whitespace();
    let date_text = words.next().ok_or("empty line")?;
    let date = Date::parse(date_text).ok_or_else(|| format!("bad date `{date_text}`"))?;
    let span_text = words.next().ok_or("missing time span")?;
    let (start, minutes) = span::parse(span_text)?;
    if minutes == 0 {
        return Err("a session must last at least a minute".to_owned());
    }
    let target = words.next().ok_or("missing project")?;
    let (project, task) = match target.split_once('/') {
        Some((project, task)) if !task.is_empty() => (project, Some(task.to_owned())),
        Some((project, _)) => (project, None),
        None => (target, None),
    };
    let project = normalise_project(project);
    if project.is_empty() {
        return Err(format!("bad project `{target}`"));
    }

    let mut tags = Vec::new();
    let mut note = Vec::new();
    for word in words {
        match Tag::parse(word) {
            Some(tag) if note.is_empty() => tags.push(tag),
            _ => note.push(word),
        }
    }
    let billable = Entry::billable_by_tags(&tags, billable_by_default);
    Ok(Entry {
        date,
        start,
        minutes,
        project,
        task,
        tags,
        note: note.join(" "),
        billable,
    })
}
