//! Reading the keyboard while a run is under way: Esc interrupts it, and a
//! line typed and entered goes to the model with its next tool results.
//!
//! The terminal is put in cbreak mode rather than raw mode: keys arrive one at
//! a time without echo, but output still turns `\n` into a new line and ctrl-c
//! still raises SIGINT, so the renderer and the existing interrupt are untouched.
//! Whoever else reads the keyboard during a run, the approval prompt, takes
//! [`hold`] first, and the listener only reads while it can take it too.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use dino_core::Inbox;
use tokio_util::sync::CancellationToken;

use crate::spinner::Status;

static KEYBOARD: Mutex<()> = Mutex::new(());

/// What the status row says interrupts a run.
#[cfg(unix)]
pub const INTERRUPT_HINT: &str = "esc to interrupt";
#[cfg(not(unix))]
pub const INTERRUPT_HINT: &str = "ctrl-c to interrupt";

/// The keyboard, for a prompt that must read it alone.
pub fn hold() -> MutexGuard<'static, ()> {
    KEYBOARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs `read` with the terminal's line editing and echo on, for a prompt that
/// reads a whole line while the listener has them off.
pub fn cooked<T>(read: impl FnOnce() -> T) -> T {
    let _restore = imp::Cooked::enter();
    read()
}

/// A running listener. Dropping it without [`Listener::stop`] still restores
/// the terminal.
pub struct Listener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<String>>,
}

impl Listener {
    /// Starts listening, or returns `None` when standard input is not a
    /// terminal or cannot be put in cbreak mode.
    #[must_use]
    pub fn start(
        cancel: CancellationToken,
        inbox: Inbox,
        status: Arc<Mutex<Status>>,
    ) -> Option<Self> {
        let terminal = imp::Cbreak::enter()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut line = Line::default();
            while !stopping.load(Ordering::Relaxed) {
                let Ok(_keyboard) = KEYBOARD.try_lock() else {
                    std::thread::sleep(imp::POLL);
                    continue;
                };
                let Some(bytes) = imp::read_ready() else {
                    continue;
                };
                for event in line.feed(&bytes) {
                    match event {
                        Key::Interrupt => cancel.cancel(),
                        Key::Entered(text) => {
                            if let Ok(mut inbox) = inbox.lock() {
                                inbox.push(text);
                            }
                        }
                    }
                }
                if let Ok(mut status) = status.lock() {
                    status.typed.clone_from(&line.text);
                    status.queued = inbox.lock().map_or(0, |inbox| inbox.len());
                }
            }
            drop(terminal);
            line.text
        });
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Stops listening and returns what was typed but not yet entered.
    pub fn stop(mut self) -> String {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Key {
    Interrupt,
    Entered(String),
}

/// The line being typed, and the bytes of an escape sequence still arriving.
#[derive(Debug, Default)]
struct Line {
    text: String,
    pending: Vec<u8>,
}

impl Line {
    /// Takes in one read's bytes. A lone Esc interrupts; an escape sequence,
    /// such as an arrow key, starts with Esc and is skipped whole.
    fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut keys = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            index += 1;
            match byte {
                0x1b => {
                    if index >= bytes.len() {
                        keys.push(Key::Interrupt);
                        continue;
                    }
                    // CSI or SS3: skip to the final byte.
                    if matches!(bytes[index], b'[' | b'O') {
                        index += 1;
                        while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                            index += 1;
                        }
                        index += 1;
                    } else {
                        index += 1;
                    }
                }
                b'\r' | b'\n' => {
                    let text = std::mem::take(&mut self.text);
                    if !text.trim().is_empty() {
                        keys.push(Key::Entered(text.trim().to_owned()));
                    }
                }
                0x7f | 0x08 => {
                    self.text.pop();
                }
                0x15 => self.text.clear(),
                byte if byte < 0x20 => {}
                _ => {
                    self.pending.push(byte);
                    if let Ok(text) = std::str::from_utf8(&self.pending) {
                        self.text.push_str(text);
                        self.pending.clear();
                    } else if self.pending.len() >= 4 {
                        self.pending.clear();
                    }
                }
            }
        }
        keys
    }
}

#[cfg(unix)]
mod imp {
    use std::os::fd::AsFd as _;
    use std::time::Duration;

    use nix::poll::PollFd;
    use nix::poll::PollFlags;
    use nix::poll::poll;
    use nix::sys::termios;

    pub const POLL: Duration = Duration::from_millis(50);

    /// The terminal in cbreak mode, put back as it was on drop.
    pub struct Cbreak(termios::Termios);

    impl Cbreak {
        pub fn enter() -> Option<Self> {
            use std::io::IsTerminal as _;
            let input = std::io::stdin();
            if !input.is_terminal() || !std::io::stdout().is_terminal() {
                return None;
            }
            let saved = termios::tcgetattr(input.as_fd()).ok()?;
            let mut cbreak = saved.clone();
            cbreak
                .local_flags
                .remove(termios::LocalFlags::ICANON | termios::LocalFlags::ECHO);
            cbreak.control_chars[termios::SpecialCharacterIndices::VMIN as usize] = 1;
            cbreak.control_chars[termios::SpecialCharacterIndices::VTIME as usize] = 0;
            termios::tcsetattr(input.as_fd(), termios::SetArg::TCSANOW, &cbreak).ok()?;
            Some(Self(saved))
        }
    }

    impl Drop for Cbreak {
        fn drop(&mut self) {
            let input = std::io::stdin();
            let _ = termios::tcsetattr(input.as_fd(), termios::SetArg::TCSANOW, &self.0);
        }
    }

    /// Line editing and echo, on until drop puts the previous mode back.
    pub struct Cooked(Option<termios::Termios>);

    impl Cooked {
        pub fn enter() -> Self {
            let input = std::io::stdin();
            let Ok(saved) = termios::tcgetattr(input.as_fd()) else {
                return Self(None);
            };
            let mut cooked = saved.clone();
            cooked
                .local_flags
                .insert(termios::LocalFlags::ICANON | termios::LocalFlags::ECHO);
            let _ = termios::tcsetattr(input.as_fd(), termios::SetArg::TCSANOW, &cooked);
            Self(Some(saved))
        }
    }

    impl Drop for Cooked {
        fn drop(&mut self) {
            if let Some(saved) = &self.0 {
                let input = std::io::stdin();
                let _ = termios::tcsetattr(input.as_fd(), termios::SetArg::TCSANOW, saved);
            }
        }
    }

    /// Whatever arrived within one poll interval, or `None`. Read from the
    /// descriptor, so nothing is left in a buffer another reader cannot see.
    pub fn read_ready() -> Option<Vec<u8>> {
        let input = std::io::stdin();
        let millis = u16::try_from(POLL.as_millis()).unwrap_or(50);
        let mut ready = [PollFd::new(input.as_fd(), PollFlags::POLLIN)];
        if !matches!(poll(&mut ready, millis), Ok(1..)) {
            return None;
        }
        let mut chunk = [0u8; 256];
        match nix::unistd::read(&input, &mut chunk) {
            Ok(read) if read > 0 => Some(chunk[..read].to_vec()),
            _ => None,
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::time::Duration;

    pub const POLL: Duration = Duration::from_millis(50);

    pub struct Cbreak;

    impl Cbreak {
        pub fn enter() -> Option<Self> {
            None
        }
    }

    pub struct Cooked;

    impl Cooked {
        pub fn enter() -> Self {
            Self
        }
    }

    pub fn read_ready() -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::Key;
    use super::Line;

    #[test]
    fn a_lone_escape_interrupts_and_an_arrow_does_not() {
        let mut line = Line::default();
        assert_eq!(line.feed(b"\x1b"), [Key::Interrupt]);
        assert!(line.feed(b"\x1b[A\x1bOB").is_empty());
        assert!(line.text.is_empty());
    }

    #[test]
    fn an_entered_line_is_queued_with_its_edits() {
        let mut line = Line::default();
        assert!(line.feed(b"use u33").is_empty());
        assert!(line.feed(b"\x7f2").is_empty());
        assert_eq!(line.feed(b"\r"), [Key::Entered("use u32".into())]);
        assert!(line.feed(b"\r").is_empty(), "an empty line is not sent");
    }

    #[test]
    fn text_split_inside_a_character_is_joined() {
        let mut line = Line::default();
        let bytes = "café".as_bytes();
        line.feed(&bytes[..4]);
        line.feed(&bytes[4..]);
        assert_eq!(line.text, "café");
    }
}
