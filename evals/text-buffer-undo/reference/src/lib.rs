use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditError {
    OutOfRange,
}

#[derive(Debug, Clone)]
enum Edit {
    Insert { at: usize, text: Vec<char> },
    Delete { at: usize, text: Vec<char> },
}

/// What the most recent edit was, while the step it made may still grow.
#[derive(Debug, Clone, Copy)]
enum Run {
    /// Typing; the next character extends it if inserted at `next`.
    Typing { next: usize },
    /// Backspacing; the next delete extends it if it ends at `start`.
    Backspace { start: usize },
    /// Deleting forward from `at`.
    Forward { at: usize },
    /// A single-character delete that could still become either.
    Single { start: usize },
}

#[derive(Debug, Default)]
pub struct Buffer {
    chars: Vec<char>,
    undo: Vec<Vec<Edit>>,
    redo: Vec<Vec<Edit>>,
    run: Option<Run>,
    depth: usize,
    /// Whether the open group has made its step yet.
    group_started: bool,
}

impl Buffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: &str) -> Self {
        Buffer { chars: text.chars().collect(), ..Self::default() }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn len_chars(&self) -> usize {
        self.chars.len()
    }

    pub fn len_lines(&self) -> usize {
        self.chars.iter().filter(|&&c| c == '\n').count() + 1
    }

    pub fn line(&self, n: usize) -> Option<String> {
        self.text().split('\n').nth(n).map(str::to_owned)
    }

    pub fn line_col(&self, at: usize) -> Result<(usize, usize), EditError> {
        if at > self.chars.len() {
            return Err(EditError::OutOfRange);
        }
        let before = &self.chars[..at];
        let line = before.iter().filter(|&&c| c == '\n').count();
        let col = before.iter().rev().take_while(|&&c| c != '\n').count();
        Ok((line, col))
    }

    pub fn offset(&self, line: usize, col: usize) -> Result<usize, EditError> {
        let mut start = 0;
        for _ in 0..line {
            let newline = self.chars[start..].iter().position(|&c| c == '\n').ok_or(EditError::OutOfRange)?;
            start += newline + 1;
        }
        let len = self.chars[start..].iter().take_while(|&&c| c != '\n').count();
        if col > len {
            return Err(EditError::OutOfRange);
        }
        Ok(start + col)
    }

    pub fn insert(&mut self, at: usize, text: &str) -> Result<(), EditError> {
        if at > self.chars.len() {
            return Err(EditError::OutOfRange);
        }
        let text: Vec<char> = text.chars().collect();
        if text.is_empty() {
            return Ok(());
        }
        self.chars.splice(at..at, text.iter().copied());
        let typed = text.len() == 1 && text[0] != '\n';
        let extends = typed && matches!(self.run, Some(Run::Typing { next }) if next == at);
        self.record(Edit::Insert { at, text }, extends);
        self.run = if typed && self.depth == 0 { Some(Run::Typing { next: at + 1 }) } else { None };
        Ok(())
    }

    pub fn delete(&mut self, range: Range<usize>) -> Result<String, EditError> {
        if range.start > range.end || range.end > self.chars.len() {
            return Err(EditError::OutOfRange);
        }
        if range.is_empty() {
            return Ok(String::new());
        }
        let text: Vec<char> = self.chars.drain(range.clone()).collect();
        let single = text.len() == 1;
        let (extends, run) = match (single, self.run) {
            (true, Some(Run::Backspace { start } | Run::Single { start })) if range.end == start => {
                (true, Some(Run::Backspace { start: range.start }))
            }
            (true, Some(Run::Forward { at } | Run::Single { start: at })) if range.start == at => {
                (true, Some(Run::Forward { at }))
            }
            (true, _) => (false, Some(Run::Single { start: range.start })),
            (false, _) => (false, None),
        };
        let deleted = text.iter().collect();
        self.record(Edit::Delete { at: range.start, text }, extends);
        self.run = if self.depth == 0 { run } else { None };
        Ok(deleted)
    }

    fn record(&mut self, edit: Edit, extends: bool) {
        self.redo.clear();
        if self.depth > 0 {
            if self.group_started {
                self.undo.last_mut().expect("the group's step").push(edit);
            } else {
                self.group_started = true;
                self.undo.push(vec![edit]);
            }
        } else if extends {
            self.undo.last_mut().expect("the step being extended").push(edit);
        } else {
            self.undo.push(vec![edit]);
        }
    }

    pub fn begin_group(&mut self) {
        self.run = None;
        if self.depth == 0 {
            self.group_started = false;
        }
        self.depth += 1;
    }

    pub fn end_group(&mut self) {
        self.run = None;
        self.depth = self.depth.saturating_sub(1);
    }

    fn close_groups(&mut self) {
        self.depth = 0;
        self.run = None;
    }

    pub fn undo(&mut self) -> bool {
        self.close_groups();
        let Some(step) = self.undo.pop() else { return false };
        for edit in step.iter().rev() {
            match edit {
                Edit::Insert { at, text } => {
                    self.chars.drain(*at..at + text.len());
                }
                Edit::Delete { at, text } => {
                    self.chars.splice(*at..*at, text.iter().copied());
                }
            }
        }
        self.redo.push(step);
        true
    }

    pub fn redo(&mut self) -> bool {
        self.close_groups();
        let Some(step) = self.redo.pop() else { return false };
        for edit in &step {
            match edit {
                Edit::Insert { at, text } => {
                    self.chars.splice(*at..*at, text.iter().copied());
                }
                Edit::Delete { at, text } => {
                    self.chars.drain(*at..at + text.len());
                }
            }
        }
        self.undo.push(step);
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}
