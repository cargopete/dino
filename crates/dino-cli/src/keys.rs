//! Keys at the prompt beyond rustyline's own: Ctrl-G edits the prompt in
//! `$EDITOR`, Ctrl-V pastes an image from the clipboard as an `@path`.
//!
//! Ctrl-R, reverse search over `.dino/history`, is rustyline's already.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use anyhow::Result;
use rustyline::Cmd;
use rustyline::ConditionalEventHandler;
use rustyline::Event;
use rustyline::EventContext;
use rustyline::RepeatCount;

/// Ctrl-G: leaves the line editor with the text so far, to be edited outside
/// it. Running an editor inside rustyline's raw mode would fight it for the
/// terminal.
pub struct ToEditor {
    pub text: Arc<Mutex<Option<String>>>,
}

impl ConditionalEventHandler for ToEditor {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, context: &EventContext) -> Option<Cmd> {
        if let Ok(mut text) = self.text.lock() {
            *text = Some(context.line().to_owned());
        }
        Some(Cmd::Interrupt)
    }
}

/// Opens `text` in `$VISUAL`, `$EDITOR` or `vi`, and returns what was saved.
///
/// # Errors
///
/// When the file cannot be written or read back, or the editor fails.
pub fn edit(text: &str) -> Result<String> {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_owned());
    edit_with(&editor, text)
}

fn edit_with(editor: &str, text: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!(
        "dino-prompt-{}-{}.md",
        std::process::id(),
        unix_millis()
    ));
    write_private(&path, text.as_bytes())?;
    // Through the shell, as git does, so `code --wait` works as a value.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status()
        .with_context(|| format!("cannot start {editor}"));
    let edited = status.and_then(|status| {
        anyhow::ensure!(status.success(), "{editor} exited with {status}");
        std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))
    });
    let _ = std::fs::remove_file(&path);
    Ok(edited?.trim_end().to_owned())
}

/// Ctrl-V: an image on the clipboard is saved and its `@path` typed in. With
/// none it does nothing; text pastes as the terminal pastes it.
pub struct PasteImage;

impl ConditionalEventHandler for PasteImage {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, _: &EventContext) -> Option<Cmd> {
        Some(match pasted_image() {
            Ok(Some(path)) => Cmd::Insert(
                1,
                format!("@{} ", crate::render::short_path(&path, Path::new(""))),
            ),
            _ => Cmd::Noop,
        })
    }
}

/// Saves the clipboard's image under Dino's state directory, outside any
/// repository, and returns where.
fn pasted_image() -> Result<Option<PathBuf>> {
    let dir = crate::settings::state_dir()
        .context("no home directory")?
        .join("pastes");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("paste-{}.png", unix_millis()));
    if clipboard_png(&path)? {
        Ok(Some(path))
    } else {
        let _ = std::fs::remove_file(&path);
        Ok(None)
    }
}

/// Writes the clipboard's image to `path` as PNG. False when it holds none.
#[cfg(target_os = "macos")]
fn clipboard_png(path: &Path) -> Result<bool> {
    // `pbpaste` gives only text. AppleScript converts whatever image the
    // pasteboard holds, a screenshot's TIFF included, and fails when none.
    const SCRIPT: &str = "on run argv
        set png to the clipboard as «class PNGf»
        set out to open for access (POSIX file (item 1 of argv)) with write permission
        set eof out to 0
        write png to out
        close access out
    end run";
    write_private(path, b"")?;
    let ran = std::process::Command::new("osascript")
        .arg("-e")
        .arg(SCRIPT)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("cannot run osascript")?;
    Ok(ran.success() && is_png(&std::fs::read(path).unwrap_or_default()))
}

#[cfg(not(target_os = "macos"))]
fn clipboard_png(path: &Path) -> Result<bool> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let (program, arguments): (&str, &[&str]) = if wayland {
        ("wl-paste", &["--no-newline", "--type", "image/png"])
    } else {
        (
            "xclip",
            &["-selection", "clipboard", "-target", "image/png", "-out"],
        )
    };
    let output = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    if !output.status.success() || !is_png(&output.stdout) {
        return Ok(false);
    }
    write_private(path, &output.stdout)?;
    Ok(true)
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .with_context(|| format!("cannot write {}", path.display()))
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_gets_the_prompt_and_its_saved_text_comes_back() {
        let edited = edit_with(
            "perl -pi -e 's/draft/final/'",
            "a draft prompt\nover two lines\n",
        );

        assert_eq!(edited.unwrap(), "a final prompt\nover two lines");
    }

    #[test]
    fn only_png_bytes_count_as_a_pasted_image() {
        assert!(is_png(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        assert!(!is_png(b"just text"));
    }
}
