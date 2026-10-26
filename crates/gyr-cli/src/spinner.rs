//! A ticking line while nothing else is being drawn: the model thinking, or a
//! slow tool running.
//!
//! A thread redraws the line four times a second. It and the renderer share
//! one lock, and the renderer stops the ticker under that lock before it writes
//! anything, so a tick can never land in the middle of real output.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use crate::style;
use crate::style::FAINT;
use crate::style::SLATE;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(250);
/// Nothing is drawn for anything quicker, so a fast step does not flicker.
const QUIET: Duration = Duration::from_secs(1);
/// Clear the line, then go to its first column. A carriage return would do
/// the second, but a terminal set to translate it turns it into a newline.
const CLEAR_LINE: &str = "\u{1b}[2K\u{1b}[1G";

#[derive(Debug)]
struct Ticking {
    since: Instant,
    /// What the line already holds, redrawn before the timer: the open call.
    prefix: String,
    /// Shown with a spinner when nothing precedes it.
    label: &'static str,
    drawn: bool,
}

#[derive(Debug, Default)]
pub struct Spinner {
    state: Arc<Mutex<Option<Ticking>>>,
    started: bool,
}

impl Spinner {
    /// Ticks on an empty line, as `⠹ thinking · 4s`.
    pub fn thinking(&mut self) {
        self.start(String::new(), "thinking");
    }

    /// Ticks after a line already written, as `› cargo test  · 12s`.
    pub fn after(&mut self, prefix: String) {
        self.start(prefix, "");
    }

    fn start(&mut self, prefix: String, label: &'static str) {
        if !style::enabled() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            *state = Some(Ticking {
                since: Instant::now(),
                prefix,
                label,
                drawn: false,
            });
        }
        if !self.started {
            self.started = true;
            let state = Arc::clone(&self.state);
            std::thread::spawn(move || tick(&state));
        }
    }

    /// Stops ticking and puts the line back as it was before the first tick:
    /// empty, or holding just its prefix.
    pub fn stop(&mut self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(ticking) = state.take()
            && ticking.drawn
        {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{CLEAR_LINE}{}", ticking.prefix);
            let _ = out.flush();
        }
    }
}

fn tick(state: &Mutex<Option<Ticking>>) {
    let mut frame = 0;
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
        frame = (frame + 1) % FRAMES.len();
        let seconds = crate::render::duration(elapsed);
        let line = if ticking.label.is_empty() {
            format!(
                "{}{}",
                ticking.prefix,
                style::paint(FAINT, &format!("  · {seconds}"))
            )
        } else {
            format!(
                "  {} {}",
                style::paint(SLATE, FRAMES[frame]),
                style::paint(FAINT, &format!("{} · {seconds}", ticking.label))
            )
        };
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "{CLEAR_LINE}{line}");
        let _ = out.flush();
        ticking.drawn = true;
    }
}
