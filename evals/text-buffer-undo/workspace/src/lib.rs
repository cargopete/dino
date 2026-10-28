use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditError {
    OutOfRange,
}

/// A text buffer with undo and redo. Positions count `char`s, not bytes.
#[derive(Debug, Default)]
pub struct Buffer {
    // Replace with whatever representation you need.
    _private: (),
}

impl Buffer {
    pub fn new() -> Self {
        todo!()
    }

    pub fn from_text(text: &str) -> Self {
        let _ = text;
        todo!()
    }

    pub fn text(&self) -> String {
        todo!()
    }

    pub fn len_chars(&self) -> usize {
        todo!()
    }

    pub fn len_lines(&self) -> usize {
        todo!()
    }

    pub fn line(&self, n: usize) -> Option<String> {
        let _ = n;
        todo!()
    }

    pub fn line_col(&self, at: usize) -> Result<(usize, usize), EditError> {
        let _ = at;
        todo!()
    }

    pub fn offset(&self, line: usize, col: usize) -> Result<usize, EditError> {
        let _ = (line, col);
        todo!()
    }

    pub fn insert(&mut self, at: usize, text: &str) -> Result<(), EditError> {
        let _ = (at, text);
        todo!()
    }

    pub fn delete(&mut self, range: Range<usize>) -> Result<String, EditError> {
        let _ = range;
        todo!()
    }

    pub fn begin_group(&mut self) {
        todo!()
    }

    pub fn end_group(&mut self) {
        todo!()
    }

    pub fn undo(&mut self) -> bool {
        todo!()
    }

    pub fn redo(&mut self) -> bool {
        todo!()
    }

    pub fn can_undo(&self) -> bool {
        todo!()
    }

    pub fn can_redo(&self) -> bool {
        todo!()
    }
}
