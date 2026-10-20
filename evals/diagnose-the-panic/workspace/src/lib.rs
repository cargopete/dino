//! Plain-text layout: word wrapping, truncation, padding and tables.

mod align;
mod table;
mod truncate;
mod width;
mod wrap;

pub use align::{pad, Align};
pub use table::render_table;
pub use truncate::truncate;
pub use width::display_width;
pub use wrap::wrap;
