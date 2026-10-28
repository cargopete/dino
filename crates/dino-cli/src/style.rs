//! The palette, and whether the terminal can take it.
//!
//! The colours are the house palette, unchanged. Its rule survives the move from
//! a page to a terminal intact: if a shell printed it, it is slate; if a person
//! wrote it, it is terracotta.
//!
//! One token is deliberately absent. The house ground is `#171614`, and a
//! line-based program does not own its background: the terminal does. Dino
//! ships the ink and leaves the ground alone, which means the palette assumes a
//! warm dark terminal because that is what it was drawn for. On a light one the
//! honest answer is `--plain` rather than a guess at what the ground is.

use std::sync::OnceLock;

static DEPTH: OnceLock<Depth> = OnceLock::new();

/// How much colour the destination will take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// Exact palette.
    True,
    /// The nearest xterm-256 approximations.
    Indexed,
    None,
}

/// One palette entry, carrying both renderings so the choice is made once at
/// startup rather than per line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    rgb: (u8, u8, u8),
    indexed: u8,
}

/// The machine's voice: tool invocations, chrome, anything a shell printed.
pub const SLATE: Colour = Colour {
    rgb: (0x8b, 0xb8, 0xdc),
    indexed: 110,
};
/// The person's voice: approval prompts, kickers, a refusal someone gave.
pub const RUST: Colour = Colour {
    rgb: (0xcd, 0x85, 0x60),
    indexed: 173,
};
/// Model prose. Never pure white; it glares on a warm dark ground.
pub const TEXT: Colour = Colour {
    rgb: (0xec, 0xe9, 0xe3),
    indexed: 254,
};
/// Chrome that still carries meaning.
pub const MUTED: Colour = Colour {
    rgb: (0x9c, 0x97, 0x8c),
    indexed: 246,
};
/// Labels a reader can lose without losing anything. Nothing load-bearing.
pub const FAINT: Colour = Colour {
    rgb: (0x6e, 0x6a, 0x61),
    indexed: 242,
};
/// A thing that worked.
pub const OK: Colour = Colour {
    rgb: (0x86, 0xb5, 0x92),
    indexed: 108,
};
/// A thing that stopped early.
pub const WARN: Colour = Colour {
    rgb: (0xcd, 0xb0, 0x6a),
    indexed: 179,
};

pub const RESET: &str = "\u{1b}[0m";
pub const BOLD: &str = "\u{1b}[1m";
pub const ITALIC: &str = "\u{1b}[3m";

/// Decides once what the destination will take.
///
/// `NO_COLOR` and `--plain` win over everything, then a dumb or absent `TERM`,
/// then `COLORTERM` for the exact palette, then the indexed approximation.
pub fn enable(force_plain: bool) {
    use std::io::IsTerminal as _;
    // Piped into a file or another program, escape sequences are noise.
    let usable = !force_plain
        && std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb");
    let depth = if !usable {
        Depth::None
    } else if std::env::var("COLORTERM")
        .is_ok_and(|value| value.contains("truecolor") || value.contains("24bit"))
    {
        Depth::True
    } else {
        Depth::Indexed
    };
    let _ = DEPTH.set(depth);
}

fn depth() -> Depth {
    *DEPTH.get().unwrap_or(&Depth::None)
}

/// Whether anything is being coloured at all.
#[must_use]
pub fn enabled() -> bool {
    depth() != Depth::None
}

/// Colour on, for a test that checks what is drawn.
#[cfg(test)]
pub fn force_colour_for_tests() {
    let _ = DEPTH.set(Depth::True);
}

impl Colour {
    /// The escape sequence that selects this colour, empty when colour is off.
    #[must_use]
    pub fn sequence(self) -> String {
        self.sequence_at(depth())
    }

    fn sequence_at(self, depth: Depth) -> String {
        match depth {
            Depth::True => {
                let (red, green, blue) = self.rgb;
                format!("\u{1b}[38;2;{red};{green};{blue}m")
            }
            Depth::Indexed => format!("\u{1b}[38;5;{}m", self.indexed),
            Depth::None => String::new(),
        }
    }
}

/// Paints a value in one colour, with optional attributes.
#[must_use]
pub fn paint(colour: Colour, value: &str) -> String {
    paint_with(colour, &[], value)
}

/// Paints a value in one colour with attributes such as [`BOLD`].
///
/// When colour is off, so are the attributes. `--plain` means no escape
/// sequences at all, not colour removed and a stray bold left behind for
/// whatever is reading the pipe.
#[must_use]
pub fn paint_with(colour: Colour, attributes: &[&str], value: &str) -> String {
    paint_at(depth(), colour, attributes, value)
}

/// The rendering, with the depth passed in rather than read from a global, so a
/// test measures the rule instead of the process's startup state.
fn paint_at(depth: Depth, colour: Colour, attributes: &[&str], value: &str) -> String {
    if depth == Depth::None {
        return value.to_owned();
    }
    format!(
        "{}{}{value}{RESET}",
        attributes.concat(),
        colour.sequence_at(depth)
    )
}

/// A mono instrument label: the stamped plate above a dial, in the house style.
///
/// Terracotta, because a label is written by a person for a person.
#[must_use]
pub fn kicker(value: &str) -> String {
    paint(RUST, &value.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_renders_exactly_in_truecolor_and_approximately_otherwise() {
        assert_eq!(
            SLATE.sequence_at(Depth::True),
            "\u{1b}[38;2;139;184;220m",
            "truecolor carries the mandated value exactly"
        );
        assert_eq!(SLATE.sequence_at(Depth::Indexed), "\u{1b}[38;5;110m");
        assert_eq!(RUST.sequence_at(Depth::True), "\u{1b}[38;2;205;133;96m");
        assert_eq!(RUST.sequence_at(Depth::Indexed), "\u{1b}[38;5;173m");
    }

    #[test]
    fn plain_emits_no_escape_sequences_at_all() {
        // Including the attributes. Colour removed with a stray bold left
        // behind is not plain, it is untidy.
        assert_eq!(paint_at(Depth::None, SLATE, &[], "tool"), "tool");
        assert_eq!(paint_at(Depth::None, RUST, &[BOLD], "approval"), "approval");
        assert_eq!(SLATE.sequence_at(Depth::None), "");
    }

    #[test]
    fn attributes_precede_the_colour_so_neither_cancels_the_other() {
        let painted = paint_at(Depth::True, RUST, &[BOLD], "approval");

        assert_eq!(painted, "\u{1b}[1m\u{1b}[38;2;205;133;96mapproval\u{1b}[0m");
    }

    #[test]
    fn the_two_accents_are_the_mandated_values() {
        // The palette is not this file's to redesign, so it is pinned here.
        assert_eq!(SLATE.rgb, (0x8b, 0xb8, 0xdc));
        assert_eq!(RUST.rgb, (0xcd, 0x85, 0x60));
        assert_eq!(TEXT.rgb, (0xec, 0xe9, 0xe3));
        assert_eq!(OK.rgb, (0x86, 0xb5, 0x92));
        assert_eq!(WARN.rgb, (0xcd, 0xb0, 0x6a));
    }
}
