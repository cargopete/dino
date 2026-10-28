//! The palette, and whether the terminal can take it.
//!
//! The colours are the house palette, unchanged. Its rule survives the move from
//! a page to a terminal intact: if a shell printed it, it is slate; if a person
//! wrote it, it is terracotta.
//!
//! One token is deliberately absent. The house ground is `#171614`, and a
//! line-based program does not own its background: the terminal does. Dino
//! ships the ink and leaves the ground alone. It asks the terminal what the
//! ground is once at startup, and on a light one draws each colour in a darker
//! ink of the same hue, because the dark palette washes out on white.

use std::sync::OnceLock;

static DEPTH: OnceLock<Depth> = OnceLock::new();
static GROUND: OnceLock<Ground> = OnceLock::new();

/// What the terminal draws behind the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ground {
    Dark,
    Light,
}

/// How much colour the destination will take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// Exact palette.
    True,
    /// The nearest xterm-256 approximations.
    Indexed,
    None,
}

/// One palette entry, carrying every rendering so the choice is made once at
/// startup rather than per line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    rgb: (u8, u8, u8),
    indexed: u8,
    /// The same hue, dark enough to read on a light ground.
    light: (u8, u8, u8),
    light_indexed: u8,
}

/// The machine's voice: tool invocations, chrome, anything a shell printed.
pub const SLATE: Colour = Colour {
    rgb: (0x8b, 0xb8, 0xdc),
    indexed: 110,
    light: (0x2f, 0x64, 0x8c),
    light_indexed: 24,
};
/// The person's voice: approval prompts, kickers, a refusal someone gave.
pub const RUST: Colour = Colour {
    rgb: (0xcd, 0x85, 0x60),
    indexed: 173,
    light: (0xa0, 0x4e, 0x2a),
    light_indexed: 130,
};
/// Model prose. Never pure white; it glares on a warm dark ground.
pub const TEXT: Colour = Colour {
    rgb: (0xec, 0xe9, 0xe3),
    indexed: 254,
    light: (0x2a, 0x28, 0x24),
    light_indexed: 235,
};
/// Chrome that still carries meaning.
pub const MUTED: Colour = Colour {
    rgb: (0x9c, 0x97, 0x8c),
    indexed: 246,
    light: (0x5c, 0x58, 0x50),
    light_indexed: 240,
};
/// Labels a reader can lose without losing anything. Nothing load-bearing.
pub const FAINT: Colour = Colour {
    rgb: (0x6e, 0x6a, 0x61),
    indexed: 242,
    light: (0x86, 0x81, 0x76),
    light_indexed: 244,
};
/// A thing that worked.
pub const OK: Colour = Colour {
    rgb: (0x86, 0xb5, 0x92),
    indexed: 108,
    light: (0x3a, 0x75, 0x4a),
    light_indexed: 65,
};
/// A thing that stopped early.
pub const WARN: Colour = Colour {
    rgb: (0xcd, 0xb0, 0x6a),
    indexed: 179,
    light: (0x8a, 0x66, 0x10),
    light_indexed: 136,
};

/// Behind a changed word in a diff, on a truecolor terminal only: removed then
/// added, on a dark ground and then a light one.
const TINTS: [[(u8, u8, u8); 2]; 2] = [
    [(0x4a, 0x2b, 0x20), (0x25, 0x3f, 0x2c)],
    [(0xf4, 0xd9, 0xcb), (0xd4, 0xec, 0xd9)],
];

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
    if depth != Depth::None {
        let _ = GROUND.set(detect_ground());
    }
}

fn depth() -> Depth {
    *DEPTH.get().unwrap_or(&Depth::None)
}

fn ground() -> Ground {
    *GROUND.get().unwrap_or(&Ground::Dark)
}

/// The terminal's own answer, then `COLORFGBG`, then the dark ground the
/// palette was drawn for.
fn detect_ground() -> Ground {
    query_background()
        .and_then(|reply| background_of(&reply))
        .or_else(|| {
            std::env::var("COLORFGBG")
                .ok()
                .and_then(|value| ground_from_colorfgbg(&value))
        })
        .unwrap_or(Ground::Dark)
}

/// Asks for the background colour (OSC 11) and then for the device
/// attributes, which every terminal answers, so one that ignores the first
/// question is known to have done so without waiting out the timeout.
#[cfg(unix)]
fn query_background() -> Option<String> {
    use std::io::IsTerminal as _;
    use std::io::Write as _;
    use std::time::Duration;
    use std::time::Instant;

    use nix::poll::PollFd;
    use nix::poll::PollFlags;
    use nix::poll::poll;

    const WAIT: Duration = Duration::from_millis(100);
    let input = std::io::stdin();
    if !input.is_terminal() {
        return None;
    }
    crossterm::terminal::enable_raw_mode().ok()?;
    let mut out = std::io::stdout();
    let asked = write!(out, "\u{1b}]11;?\u{1b}\\\u{1b}[c").and_then(|()| out.flush());
    let mut reply = Vec::new();
    let deadline = Instant::now() + WAIT;
    while asked.is_ok() && !answered_attributes(&reply) {
        let left = deadline.saturating_duration_since(Instant::now());
        let millis = u16::try_from(left.as_millis()).unwrap_or(u16::MAX);
        let mut ready = [PollFd::new(
            std::os::fd::AsFd::as_fd(&input),
            PollFlags::POLLIN,
        )];
        if left.is_zero() || !matches!(poll(&mut ready, millis), Ok(1..)) {
            break;
        }
        // From the descriptor, because the buffered `Stdin` would keep any
        // bytes typed after the reply from the line editor.
        let mut chunk = [0u8; 256];
        match nix::unistd::read(&input, &mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => reply.extend_from_slice(&chunk[..read]),
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();
    Some(String::from_utf8_lossy(&reply).into_owned())
}

#[cfg(not(unix))]
fn query_background() -> Option<String> {
    None
}

/// Whether the reply holds the device-attributes answer, `ESC [ ? … c`.
fn answered_attributes(reply: &[u8]) -> bool {
    reply
        .windows(3)
        .position(|window| window == b"\x1b[?")
        .is_some_and(|start| reply[start..].contains(&b'c'))
}

/// The ground an OSC 11 reply describes: `rgb:R/G/B`, each channel one to
/// four hex digits.
fn background_of(reply: &str) -> Option<Ground> {
    let start = reply.find("]11;rgb:")? + "]11;rgb:".len();
    let channels: Vec<f64> = reply[start..]
        .split(['\u{7}', '\u{1b}'])
        .next()?
        .split('/')
        .map(|hex| {
            let digits = u32::try_from(hex.len())
                .ok()
                .filter(|digits| (1..=4).contains(digits))?;
            let value = u32::from_str_radix(hex, 16).ok()?;
            Some(f64::from(value) / f64::from((1u32 << (4 * digits)) - 1))
        })
        .collect::<Option<_>>()?;
    let [red, green, blue] = channels[..] else {
        return None;
    };
    let luminance = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
    Some(if luminance > 0.5 {
        Ground::Light
    } else {
        Ground::Dark
    })
}

/// `COLORFGBG` as rxvt and its descendants set it, `fg;bg`, where backgrounds
/// 7 and 9 to 15 are the light ones.
fn ground_from_colorfgbg(value: &str) -> Option<Ground> {
    let background: u8 = value.rsplit(';').next()?.parse().ok()?;
    Some(if background == 7 || (9..=15).contains(&background) {
        Ground::Light
    } else {
        Ground::Dark
    })
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
        self.sequence_on(depth, ground())
    }

    fn sequence_on(self, depth: Depth, ground: Ground) -> String {
        let (rgb, indexed) = match ground {
            Ground::Dark => (self.rgb, self.indexed),
            Ground::Light => (self.light, self.light_indexed),
        };
        match depth {
            Depth::True => {
                let (red, green, blue) = rgb;
                format!("\u{1b}[38;2;{red};{green};{blue}m")
            }
            Depth::Indexed => format!("\u{1b}[38;5;{indexed}m"),
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

/// A changed word in a diff: on a tinted ground where the terminal has exact
/// colour, else bold, rather than a guessed shade from the 256.
#[must_use]
pub fn paint_changed(colour: Colour, added: bool, value: &str) -> String {
    match depth() {
        Depth::None => value.to_owned(),
        Depth::Indexed => paint_with(colour, &[BOLD], value),
        Depth::True => {
            let (red, green, blue) =
                TINTS[usize::from(ground() == Ground::Light)][usize::from(added)];
            format!(
                "\u{1b}[48;2;{red};{green};{blue}m{}{value}{RESET}",
                colour.sequence_at(Depth::True)
            )
        }
    }
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
    fn a_light_ground_gets_the_darker_ink() {
        assert_eq!(
            SLATE.sequence_on(Depth::True, Ground::Light),
            "\u{1b}[38;2;47;100;140m"
        );
        assert_eq!(
            SLATE.sequence_on(Depth::Indexed, Ground::Light),
            "\u{1b}[38;5;24m"
        );
        assert_eq!(
            SLATE.sequence_on(Depth::True, Ground::Dark),
            "\u{1b}[38;2;139;184;220m"
        );
    }

    #[test]
    fn the_ground_is_read_from_the_terminal_reply() {
        let white = "\u{1b}]11;rgb:ffff/ffff/ffff\u{1b}\\\u{1b}[?62;22c";
        assert_eq!(background_of(white), Some(Ground::Light));
        let house = "\u{1b}]11;rgb:1717/1616/1414\u{7}";
        assert_eq!(background_of(house), Some(Ground::Dark));
        assert_eq!(
            background_of("\u{1b}]11;rgb:f/f/e\u{7}"),
            Some(Ground::Light)
        );
        // A terminal that answered only the attributes question.
        assert_eq!(background_of("\u{1b}[?1;2c"), None);
        assert_eq!(background_of("\u{1b}]11;rgb:zz/00/00\u{7}"), None);
    }

    #[test]
    fn the_wait_ends_at_the_attributes_reply() {
        assert!(answered_attributes(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;c"));
        assert!(!answered_attributes(b"\x1b]11;rgb:0/0/0\x07\x1b[?62"));
    }

    #[test]
    fn colorfgbg_names_light_backgrounds() {
        assert_eq!(ground_from_colorfgbg("0;15"), Some(Ground::Light));
        assert_eq!(ground_from_colorfgbg("0;default;7"), Some(Ground::Light));
        assert_eq!(ground_from_colorfgbg("15;0"), Some(Ground::Dark));
        assert_eq!(ground_from_colorfgbg("15;8"), Some(Ground::Dark));
        assert_eq!(ground_from_colorfgbg(""), None);
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
