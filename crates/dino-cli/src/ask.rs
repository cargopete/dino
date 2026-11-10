//! The model's questions at the terminal: one key per option, as the approval
//! prompt takes, and a line of the person's own for anything else.

use std::fmt::Write as _;
use std::io::BufRead;
use std::io::IsTerminal;
use std::io::Write;
use std::io::stderr;
use std::io::stdin;
use std::time::Duration;

use dino_core::ask::Answer;
use dino_core::ask::AnswerFuture;
use dino_core::ask::Answered;
use dino_core::ask::Asker;
use dino_core::ask::Question;

use crate::style;
use crate::style::BOLD;
use crate::style::EMBER;
use crate::style::FAINT;

/// The key that writes an answer of one's own.
const OTHER: char = 'o';

#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalAsker;

impl Asker for TerminalAsker {
    fn ask(&self, questions: Vec<Question>) -> AnswerFuture<'_> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || ask_on_terminal(&questions))
                .await
                .ok()
                .flatten()
        })
    }
}

/// `None` when there is no terminal to ask at, which the model is told as no
/// one being there.
fn ask_on_terminal(questions: &[Question]) -> Option<Answered> {
    if !stdin().is_terminal() {
        return None;
    }
    let _keyboard = crate::listen::hold();
    crate::render::others_wrote();
    let answers = questions.iter().map(ask_one).collect::<Option<Vec<_>>>()?;
    Some(Answered {
        by: "terminal".to_owned(),
        answers,
    })
}

/// The lines a question is shown as, the recommended option first.
fn lines(question: &Question) -> Vec<String> {
    let mut lines: Vec<String> = question
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            let mut line = format!("[{}] {}", index + 1, option.label);
            if index == 0 {
                line.push_str(" (recommended)");
            }
            if let Some(description) = &option.description {
                let _ = write!(line, " · {description}");
            }
            line
        })
        .collect();
    lines.push(format!("[{OTHER}] something else"));
    lines
}

fn ask_one(question: &Question) -> Option<Answer> {
    let header = style::paint_with(EMBER, &[BOLD], "  question");
    let hint = if question.multi_select {
        "keys pick any that apply, enter sends, esc leaves it unanswered"
    } else {
        "a key picks, esc leaves it unanswered"
    };
    let mut err = stderr().lock();
    writeln!(err, "\n{header}  {}", question.question).ok()?;
    writeln!(err, "{}", style::paint(FAINT, &format!("      {hint}"))).ok()?;
    err.flush().ok()?;
    drop(err);
    let lines = lines(question);
    let picked = if stderr().is_terminal() {
        pick(&lines, question.multi_select)
    } else {
        None
    };
    let picked = match picked {
        Some(picked) => picked,
        None => typed(&lines, question.options.len())?,
    };
    let other = picked
        .other
        .then(|| {
            let mut err = stderr();
            let _ = write!(err, "{}", style::paint(FAINT, "      your answer: "));
            let _ = err.flush();
            let mut line = String::new();
            let _ = crate::listen::cooked(|| stdin().lock().read_line(&mut line));
            line.trim().to_owned()
        })
        .filter(|line| !line.is_empty());
    Some(Answer {
        question: question.question.clone(),
        chosen: picked
            .options
            .iter()
            .map(|&index| question.options[index].label.clone())
            .collect(),
        other,
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Picked {
    options: Vec<usize>,
    other: bool,
}

/// The selector. `None` when the terminal cannot go raw.
fn pick(lines: &[String], multi: bool) -> Option<Picked> {
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEventKind;
    use crossterm::event::KeyModifiers;
    use crossterm::event::poll;
    use crossterm::event::read;
    use crossterm::terminal;

    terminal::enable_raw_mode().ok()?;
    let raw = RawGuard;
    while poll(Duration::ZERO).unwrap_or(false) {
        let _ = read();
    }
    let options = lines.len() - 1;
    let mut marked = vec![false; options];
    let mut drawn = false;
    let picked = loop {
        draw(lines, &marked, drawn);
        drawn = true;
        let Ok(Event::Key(key)) = read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                break Picked::default();
            }
            KeyCode::Esc => break Picked::default(),
            KeyCode::Enter if multi => break chosen(&marked, false),
            KeyCode::Char(OTHER) => break chosen(&marked, true),
            KeyCode::Char(typed) => {
                let Some(index) = typed
                    .to_digit(10)
                    .and_then(|digit| (digit as usize).checked_sub(1))
                    .filter(|&index| index < options)
                else {
                    continue;
                };
                if !multi {
                    break chosen(&marked, false).with(index);
                }
                marked[index] = !marked[index];
            }
            _ => {}
        }
    };
    let said = describe(&picked, lines);
    let mut err = stderr();
    let _ = write!(
        err,
        "\x1b[{}A\r\x1b[J{}\r\n",
        lines.len(),
        style::paint(FAINT, &format!("      › {said}"))
    );
    let _ = err.flush();
    drop(raw);
    Some(picked)
}

fn chosen(marked: &[bool], other: bool) -> Picked {
    Picked {
        options: (0..marked.len()).filter(|&index| marked[index]).collect(),
        other,
    }
}

impl Picked {
    fn with(mut self, index: usize) -> Self {
        self.options = vec![index];
        self
    }
}

fn describe(picked: &Picked, lines: &[String]) -> String {
    let mut said: Vec<&str> = picked
        .options
        .iter()
        .map(|&index| lines[index].as_str())
        .collect();
    if picked.other {
        said.push("something else");
    }
    if said.is_empty() {
        "left unanswered".to_owned()
    } else {
        said.join(", ")
    }
}

fn draw(lines: &[String], marked: &[bool], redraw: bool) {
    let mut text = String::new();
    if redraw {
        let _ = write!(text, "\x1b[{}A", lines.len());
    }
    for (index, line) in lines.iter().enumerate() {
        let line = if marked.get(index).copied().unwrap_or(false) {
            style::paint_with(EMBER, &[BOLD], &format!("    › {line}"))
        } else {
            style::paint(FAINT, &format!("      {line}"))
        };
        let _ = write!(text, "\r\x1b[2K{line}\r\n");
    }
    let mut err = stderr();
    let _ = write!(err, "{text}");
    let _ = err.flush();
}

/// For a terminal that cannot go raw: one typed line of keys, such as `1` or
/// `13`, or `o` for an answer of one's own.
fn typed(lines: &[String], options: usize) -> Option<Picked> {
    let mut err = stderr().lock();
    let _ = write!(
        err,
        "{}",
        style::paint(FAINT, &format!("      {}: ", lines.join("   ")))
    );
    let _ = err.flush();
    drop(err);
    let mut line = String::new();
    if stdin().lock().read_line(&mut line).ok()? == 0 {
        return None;
    }
    Some(parse_typed(&line, options))
}

fn parse_typed(line: &str, options: usize) -> Picked {
    let mut picked = Picked::default();
    for typed in line.trim().chars() {
        if typed.eq_ignore_ascii_case(&OTHER) {
            picked.other = true;
        } else if let Some(index) = typed
            .to_digit(10)
            .and_then(|digit| (digit as usize).checked_sub(1))
            .filter(|&index| index < options && !picked.options.contains(&index))
        {
            picked.options.push(index);
        }
    }
    picked
}

/// Restores the terminal however the selector exits.
struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dino_core::ask::Choice;

    #[test]
    fn the_recommended_option_is_first_and_marked() {
        let question = Question {
            question: "Which error type?".into(),
            options: vec![
                Choice {
                    label: "thiserror".into(),
                    description: Some("for a library".into()),
                },
                Choice {
                    label: "anyhow".into(),
                    description: None,
                },
            ],
            multi_select: false,
        };
        assert_eq!(
            lines(&question),
            [
                "[1] thiserror (recommended) · for a library",
                "[2] anyhow",
                "[o] something else",
            ]
        );
    }

    #[test]
    fn a_typed_line_picks_options_by_number_and_ignores_the_rest() {
        assert_eq!(
            parse_typed("13o9x\n", 3),
            Picked {
                options: vec![0, 2],
                other: true
            }
        );
        assert_eq!(parse_typed("\n", 3), Picked::default());
    }
}
