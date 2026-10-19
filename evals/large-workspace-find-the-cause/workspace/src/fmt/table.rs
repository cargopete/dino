use crate::fmt::pad::{pad_left, pad_right, truncate, width};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// A text table with columns padded to their widest cell and separated by
/// two spaces.
#[derive(Debug, Clone)]
pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
    max_width: usize,
}

impl Table {
    #[must_use]
    pub fn new(headers: &[&str], aligns: &[Align]) -> Self {
        assert_eq!(headers.len(), aligns.len(), "one alignment per column");
        Self {
            headers: headers.iter().map(|header| (*header).to_owned()).collect(),
            aligns: aligns.to_vec(),
            rows: Vec::new(),
            max_width: 32,
        }
    }

    pub fn push(&mut self, row: Vec<String>) {
        assert_eq!(row.len(), self.headers.len(), "one cell per column");
        self.rows.push(row);
    }

    #[must_use]
    pub fn render(&self) -> String {
        let cells: Vec<Vec<String>> = std::iter::once(&self.headers)
            .chain(&self.rows)
            .map(|row| row.iter().map(|cell| truncate(cell, self.max_width)).collect())
            .collect();
        let widths: Vec<usize> = (0..self.headers.len())
            .map(|column| cells.iter().map(|row| width(&row[column])).max().unwrap_or(0))
            .collect();
        let mut out = String::new();
        for row in &cells {
            let line: Vec<String> = row
                .iter()
                .zip(&widths)
                .zip(&self.aligns)
                .map(|((cell, width), align)| match align {
                    Align::Left => pad_right(cell, *width),
                    Align::Right => pad_left(cell, *width),
                })
                .collect();
            out.push_str(line.join("  ").trim_end());
            out.push('\n');
        }
        out
    }
}
