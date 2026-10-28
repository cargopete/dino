//! The status row: one line beneath the transcript while the model thinks or a
//! call runs, saying what is happening, for how long, and what it has cost.
//!
//! A thread redraws it twenty times a second. It and the renderer share one
//! lock, and the renderer stops the ticker under that lock before it writes
//! anything, so a tick can never land in the middle of real output. The row
//! never wraps, so clearing its one line leaves nothing behind.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use crate::style;
use crate::style::FAINT;
use crate::style::SLATE;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(50);
/// How long each frame is shown, slower than the redraw so the dots read as
/// turning rather than as a flicker.
const FRAME: Duration = Duration::from_millis(80);
/// Nothing is drawn for anything quicker, so a fast step does not flicker.
const QUIET: Duration = Duration::from_secs(1);
/// Clear the line, then go to its first column. A carriage return would do
/// the second, but a terminal set to translate it turns it into a newline.
const CLEAR_LINE: &str = "\u{1b}[2K\u{1b}[1G";

/// The figures the row shows, kept by the renderer and the session.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub model: String,
    pub window: Option<u32>,
    /// Input tokens of the latest request, which is how full the context is.
    pub context: u64,
    /// This submission's tokens so far.
    pub input: u64,
    pub output: u64,
}

impl Status {
    /// The model answering, named without the provider in its display name.
    pub fn answering(&mut self, profile: &dino_protocol::ModelProfile) {
        let name = &profile.display_name;
        name.split(" (")
            .next()
            .unwrap_or(name)
            .clone_into(&mut self.model);
        self.window = profile.context_window_tokens;
    }
}

#[derive(Debug)]
struct Ticking {
    since: Instant,
    label: &'static str,
    drawn: bool,
}

#[derive(Debug, Default)]
pub struct Spinner {
    state: Arc<Mutex<Option<Ticking>>>,
    status: Arc<Mutex<Status>>,
    started: bool,
}

impl Spinner {
    #[must_use]
    pub fn new(status: Arc<Mutex<Status>>) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    /// Ticks on an empty line, as `⠹ thinking 4s · …`.
    pub fn thinking(&mut self) {
        self.start("thinking");
    }

    /// Ticks on the line beneath a running call.
    pub fn running(&mut self) {
        self.start("running");
    }

    fn start(&mut self, label: &'static str) {
        if !style::enabled() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            *state = Some(Ticking {
                since: Instant::now(),
                label,
                drawn: false,
            });
        }
        if !self.started {
            self.started = true;
            let state = Arc::clone(&self.state);
            let status = Arc::clone(&self.status);
            std::thread::spawn(move || tick(&state, &status));
        }
    }

    /// Stops ticking and leaves the row's line empty.
    pub fn stop(&mut self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(ticking) = state.take()
            && ticking.drawn
        {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{CLEAR_LINE}");
            let _ = out.flush();
        }
    }
}

fn tick(state: &Mutex<Option<Ticking>>, status: &Mutex<Status>) {
    loop {
        std::thread::sleep(TICK);
        let Ok(mut guard) = state.lock() else {
            return;
        };
        let Some(ticking) = guard.as_mut() else {
            continue;
        };
        let elapsed = ticking.since.elapsed();
        if elapsed < QUIET {
            continue;
        }
        let figures = status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default();
        let frames = usize::try_from(elapsed.as_millis() / FRAME.as_millis()).unwrap_or(0);
        let frame = FRAMES[frames % FRAMES.len()];
        let room = crate::markdown::terminal_width().saturating_sub(5);
        let line = format!(
            "  {} {}",
            style::paint(SLATE, frame),
            style::paint(FAINT, &row(ticking.label, elapsed, &figures, room))
        );
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "{CLEAR_LINE}{line}");
        let _ = out.flush();
        ticking.drawn = true;
    }
}

/// The row after its spinner, as many of its parts as fit in `room` columns.
/// The parts go in the order a person reads them and are dropped in the order
/// they matter least: the tokens, the model, the context and then the key.
fn row(label: &str, elapsed: Duration, status: &Status, room: usize) -> String {
    let mut parts: Vec<(u8, String)> = vec![(
        u8::MAX,
        format!("{label} {}", crate::render::duration(elapsed)),
    )];
    if !status.model.is_empty() {
        parts.push((1, status.model.clone()));
    }
    if let Some(window) = status.window.filter(|_| status.context > 0) {
        let percent = status.context * 100 / u64::from(window).max(1);
        parts.push((2, format!("{percent}% context")));
    }
    if status.input + status.output > 0 {
        parts.push((
            0,
            format!(
                "{} in · {} out",
                crate::render::compact(status.input),
                crate::render::compact(status.output)
            ),
        ));
    }
    parts.push((3, "ctrl-c to interrupt".to_owned()));
    loop {
        let line = parts
            .iter()
            .map(|(_, part)| part.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        if crate::calls::width(&line) <= room || parts.len() == 1 {
            return crate::calls::cut(&line, room);
        }
        if let Some(least) = parts
            .iter()
            .enumerate()
            .min_by_key(|(_, (rank, _))| *rank)
            .map(|(index, _)| index)
        {
            parts.remove(least);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        Status {
            model: "Qwen3.7-Max".into(),
            window: Some(262_144),
            context: 65_536,
            input: 18_200,
            output: 1_100,
        }
    }

    #[test]
    fn the_row_says_what_the_turn_is_doing_and_what_it_has_cost() {
        assert_eq!(
            row("thinking", Duration::from_secs(4), &status(), 200),
            "thinking 4s · Qwen3.7-Max · 25% context · 18.2k in · 1.1k out · ctrl-c to interrupt"
        );
    }

    #[test]
    fn a_narrow_row_drops_what_matters_least_first() {
        let row = |room| row("running", Duration::from_secs(72), &status(), room);
        assert_eq!(
            row(70),
            "running 1m 12s · Qwen3.7-Max · 25% context · ctrl-c to interrupt"
        );
        assert_eq!(
            row(50),
            "running 1m 12s · 25% context · ctrl-c to interrupt"
        );
        assert_eq!(row(36), "running 1m 12s · ctrl-c to interrupt");
        assert_eq!(row(10), "running 1…");
    }

    #[test]
    fn figures_not_yet_known_are_left_out() {
        let fresh = Status {
            model: "Qwen3.7-Max".into(),
            window: Some(262_144),
            ..Status::default()
        };
        assert_eq!(
            row("thinking", Duration::from_secs(1), &fresh, 200),
            "thinking 1s · Qwen3.7-Max · ctrl-c to interrupt"
        );
    }
}
