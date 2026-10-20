use crate::error::Error;

/// One `key = value` line, with the section it sits under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub section: String,
    pub key: String,
    pub value: String,
    pub line: usize,
}

/// Splits config text into settings. Keys before any section header belong to
/// the empty section. A `#` starts a comment anywhere outside a quoted value.
pub fn parse(text: &str) -> Result<Vec<Setting>, Error> {
    let mut section = String::new();
    let mut settings = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let content = strip_comment(raw).trim();
        if content.is_empty() {
            continue;
        }
        if let Some(rest) = content.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or_else(|| Error::Config {
                line,
                message: "section header is missing its closing bracket".to_owned(),
            })?;
            section = name.trim().to_ascii_lowercase();
            continue;
        }
        let (key, value) = content.split_once('=').ok_or_else(|| Error::Config {
            line,
            message: format!("expected `key = value`, found `{content}`"),
        })?;
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err(Error::Config {
                line,
                message: "empty key".to_owned(),
            });
        }
        settings.push(Setting {
            section: section.clone(),
            key,
            value: unquote(value.trim()).to_owned(),
            line,
        });
    }
    Ok(settings)
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (at, character) in line.char_indices() {
        match character {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..at],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
}
