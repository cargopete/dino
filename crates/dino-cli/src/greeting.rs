//! The dinosaur an interactive session opens with.
//!
//! It is drawn only in colour. Without colour the art is noise to a screen
//! reader and to whatever reads a pipe, so the plain opening stands alone.

use std::fmt::Write as _;

use crate::style;
use crate::style::EMBER;
use crate::style::FAINT;
use crate::style::HIDE;
use crate::style::MUTED;
use crate::style::PURPLE;
use crate::style::SPOT;
use crate::style::TEXT;

/// Each line of the drawing and, beneath it, which ink every column takes:
/// `p` purple hide, `s` sandstone plates, `o` a spot, `e` the eye, `g` the
/// stone it stands on.
const ART: [(&str, &str); 9] = [
    (
        r"              /\    /\    /\",
        r"              ss    ss    ss",
    ),
    (
        r"            _/  \__/  \__/  \_           .--.",
        r"            ss  ssss  ssss  ss           pppp",
    ),
    (
        r"         .-'                  '-.       / o  \",
        r"         ppp                  ppp       p e  p",
    ),
    (
        r"     _.-'   o       o       o    '-----'   .-'",
        r"     pppp   o       o       o    ppppppp   ppp",
    ),
    (
        r"  <-'__        o        o             ._.-'",
        r"  ppppp        o        o             ppppp",
    ),
    (
        r"       '--.__   ___________   ___.--''",
        r"       pppppp   ppppppppppp   pppppppp",
    ),
    (
        r"             | |           | |",
        r"             p p           p p",
    ),
    (
        r"            _| |_         _| |_",
        r"            pp pp         pp pp",
    ),
    (
        r" ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~",
        r" gggggggggggggggggggggggggggggggggggggggggggggggg",
    ),
];

/// Whether a session opens with the drawing: asked for, in colour, and with
/// a person at the keyboard rather than a script feeding standard input.
pub fn shows(wanted: bool, colour: bool, interactive: bool) -> bool {
    wanted && colour && interactive
}

/// The drawing and a one-line hello beneath it.
#[must_use]
pub fn greeting(model: &str, workspace: &str) -> String {
    let mut drawn = String::from("\n");
    for (line, inks) in ART {
        let mut start = 0;
        while start < line.len() {
            let ink = inks.as_bytes()[start];
            let end = inks
                .bytes()
                .skip(start)
                .position(|other| other != ink)
                .map_or(line.len(), |run| start + run);
            let run = &line[start..end];
            drawn.push_str(&match ink {
                b'p' => style::paint(PURPLE, run),
                b's' => style::paint(MUTED, run),
                b'o' => style::paint_on(SPOT, HIDE, run),
                b'e' => style::paint_with(EMBER, &[style::BOLD], run),
                b'g' => style::paint(FAINT, run),
                _ => run.to_owned(),
            });
            start = end;
        }
        drawn.push('\n');
    }
    let _ = writeln!(
        drawn,
        "  {} {}  {}",
        style::paint_with(EMBER, &[style::BOLD], "dino"),
        style::paint(FAINT, env!("CARGO_PKG_VERSION")),
        style::paint(TEXT, &format!("{model} · {workspace}")),
    );
    drawn
}

/// Every ink by role, then the greeting, for `dino palette`.
#[must_use]
pub fn palette() -> String {
    let inks = [
        (
            PURPLE,
            "purple",
            "the machine's voice: tools, chrome, what a shell printed",
        ),
        (
            EMBER,
            "ember",
            "the person's voice: prompts, labels, approvals",
        ),
        (TEXT, "text", "model prose"),
        (
            MUTED,
            "muted",
            "sandstone: chrome that still carries meaning",
        ),
        (FAINT, "faint", "stone: labels a reader can lose"),
        (style::OK, "ok", "a thing that worked"),
        (style::WARN, "warn", "a thing that stopped early"),
    ];
    let mut shown = String::new();
    for (ink, name, role) in inks {
        let _ = writeln!(
            shown,
            "  {}  {role}",
            style::paint(ink, &format!("{name:<6}"))
        );
    }
    let _ = writeln!(
        shown,
        "  {}  {}",
        style::paint_on(SPOT, HIDE, "spot  "),
        style::paint(FAINT, "a spot on the hide, carrying its own ground")
    );
    if style::enabled() {
        shown.push_str(&greeting("Model", "~/workspace"));
    } else {
        shown.push_str("  no colour here, so no drawing\n");
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_has_an_ink_and_the_drawing_fits() {
        for (line, inks) in ART {
            assert!(line.is_ascii(), "{line}");
            assert_eq!(line.len(), inks.len(), "{line}");
            assert!(line.len() < 60, "{line}");
            for (glyph, ink) in line.chars().zip(inks.chars()) {
                assert_eq!(glyph == ' ', ink == ' ', "{line}");
                assert!("psoeg ".contains(ink), "{inks}");
            }
        }
    }

    #[test]
    fn it_shows_only_when_wanted_in_colour_to_a_person() {
        assert!(shows(true, true, true));
        assert!(!shows(false, true, true), "turned off in config");
        assert!(!shows(true, false, true), "--plain, NO_COLOR or a pipe");
        assert!(
            !shows(true, true, false),
            "standard input is not a terminal"
        );
    }

    #[test]
    fn the_hello_names_the_model_and_the_workspace() {
        let text = greeting("Claude Opus 5.5", "~/dino");

        assert!(text.contains("Claude Opus 5.5 · ~/dino"), "{text}");
        assert_eq!(text.lines().count(), ART.len() + 2);
    }
}
