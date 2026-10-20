use crate::{display_width, pad, truncate, Align};

/// Renders a header row, a rule and the body rows as aligned columns.
///
/// Each column is as wide as its widest cell, header included, but never wider
/// than `max_width`; longer cells are truncated.
///
/// # Panics
///
/// If `aligns` or any row has a different length from `headers`.
#[must_use]
pub fn render_table(
    headers: &[&str],
    rows: &[Vec<String>],
    aligns: &[Align],
    max_width: usize,
) -> String {
    assert_eq!(aligns.len(), headers.len(), "one alignment per column");
    for row in rows {
        assert_eq!(row.len(), headers.len(), "one cell per column");
    }

    let widths: Vec<usize> = (0..headers.len())
        .map(|col| {
            let widest = rows
                .iter()
                .map(|row| row[col].len())
                .chain(std::iter::once(display_width(headers[col])))
                .max()
                .unwrap_or(0);
            widest.min(max_width)
        })
        .collect();

    let render_row = |cells: Vec<&str>| -> String {
        cells
            .iter()
            .zip(&widths)
            .zip(aligns)
            .map(|((cell, &width), &align)| pad(&truncate(cell, width), width, align))
            .collect::<Vec<_>>()
            .join(" | ")
    };

    let mut lines = vec![render_row(headers.to_vec())];
    lines.push(
        widths
            .iter()
            .map(|&w| "-".repeat(w))
            .collect::<Vec<_>>()
            .join("-+-"),
    );
    for row in rows {
        lines.push(render_row(row.iter().map(String::as_str).collect()));
    }
    lines.join("\n")
}
