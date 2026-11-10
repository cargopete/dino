//! `/rewind` and `/fork`: checkpoints listed, what going back would do, going
//! back, and carrying on in a copy.

use std::fmt::Write as _;

use dino_core::checkpoint::Checkpoint;
use dino_core::checkpoint::Left;
use dino_core::checkpoint::Plan;
use dino_core::checkpoint::Rewound;
use dino_core::checkpoint::Why;
use dino_model::ModelSession;

use crate::session::Session;
use crate::style;
use crate::style::FAINT;
use crate::style::MUTED;
use crate::style::PURPLE;

/// Said wherever files are rewound, since nothing else would say it.
const IGNORED: &str = "  ignored files, such as target/, were never captured and stay as they are";

/// Checkpoints listed, newest last, the latest few of a long conversation.
const LISTED: usize = 20;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn first_line(prompt: &str) -> String {
    dino_core::text::capped_head(prompt.lines().next().unwrap_or_default().trim(), 80)
}

/// The conversation's checkpoints, one line each.
pub(crate) fn listing(checkpoints: &[Checkpoint], now: u64) -> String {
    if checkpoints.is_empty() {
        return style::paint(
            FAINT,
            "  no checkpoints yet; one is taken before each message\n",
        );
    }
    let mut block = String::new();
    for checkpoint in &checkpoints[checkpoints.len().saturating_sub(LISTED)..] {
        let age = crate::session::describe_age(now.saturating_sub(checkpoint.unix_seconds));
        let _ = writeln!(
            &mut block,
            "  {}  {}  {}",
            style::paint(PURPLE, &format!("{:>3}", checkpoint.number)),
            style::paint(MUTED, &format!("{age:>4} ago")),
            first_line(&checkpoint.prompt)
        );
    }
    block.push_str(&style::paint(
        FAINT,
        "  /rewind <n> goes back to before that message: the conversation, the files or both\n",
    ));
    block
}

fn paths(paths: &[String]) -> String {
    const SHOWN: usize = 12;
    let mut text = paths
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > SHOWN {
        let _ = write!(text, " and {} more", paths.len() - SHOWN);
    }
    text
}

fn left(left: &[Left], why: Why) -> Vec<String> {
    left.iter()
        .filter(|left| left.why == why)
        .map(|left| left.path.clone())
        .collect()
}

fn describe_plan(block: &mut String, plan: &Plan, done: bool) {
    let (restores, removes) = if done {
        ("restored", "removed")
    } else {
        ("restores", "removes")
    };
    if plan.is_empty() {
        block.push_str("  files: nothing this conversation changed differs from then\n");
    }
    if !plan.restore.is_empty() {
        let _ = writeln!(block, "  files: {restores} {}", paths(&plan.restore));
    }
    if !plan.remove.is_empty() {
        let _ = writeln!(block, "  files: {removes} {}", paths(&plan.remove));
    }
    for (why, said) in [
        (
            Why::ChangedSince,
            "changed since this conversation last did",
        ),
        (Why::NotTouched, "not changed by this conversation"),
        (Why::Submodule, "submodules"),
    ] {
        let named = left(&plan.left, why);
        if !named.is_empty() {
            let _ = writeln!(
                block,
                "{}",
                style::paint(
                    style::WARN,
                    &format!("  left alone, {said}: {}", paths(&named))
                )
            );
        }
    }
}

/// What going back to before `checkpoint` would do, and the choice.
pub(crate) fn preview(
    checkpoint: &Checkpoint,
    plan: &Plan,
    kept_conversation: bool,
    now: u64,
) -> String {
    let age = crate::session::describe_age(now.saturating_sub(checkpoint.unix_seconds));
    let mut block = format!(
        "  before {} ({age} ago): {}\n",
        checkpoint.number,
        first_line(&checkpoint.prompt)
    );
    describe_plan(&mut block, plan, false);
    block.push_str(&style::paint(FAINT, IGNORED));
    block.push('\n');
    if !kept_conversation {
        block.push_str(&style::paint(
            style::WARN,
            "  the conversation as it was then was not kept, so only the files can go back\n",
        ));
    }
    block.push_str(&style::paint(
        PURPLE,
        "  rewind: [c] conversation   [f] files   [b] both   or anything else to leave it\n",
    ));
    block
}

/// What a files rewind did.
pub(crate) fn report(rewound: &Rewound) -> String {
    let mut block = String::new();
    describe_plan(&mut block, &rewound.plan, true);
    for (path, error) in &rewound.failed {
        let _ = writeln!(
            block,
            "{}",
            style::paint(style::WARN, &format!("  could not restore {path}: {error}"))
        );
    }
    block
}

impl<S: ModelSession> Session<S> {
    /// Records the workspace and the conversation before a submission, unless
    /// it is a plan, which changes nothing. Outside git there is nothing to do.
    pub(crate) fn checkpoint(&self, typed: &str) -> Option<u32> {
        if self.planning.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let id = self.current_id();
        let number = match dino_core::checkpoint::begin(&self.workspace_root, &id, typed) {
            Ok(number) => number,
            Err(dino_core::checkpoint::CheckpointError::NotARepository) => return None,
            Err(error) => {
                println!(
                    "{}",
                    style::paint(style::WARN, &format!("  no checkpoint: {error}"))
                );
                return None;
            }
        };
        self.keep_conversation(number);
        Some(number)
    }

    /// Keeps the conversation as it is now beside checkpoint `number`.
    fn keep_conversation(&self, number: u32) {
        let path =
            dino_core::checkpoint::state_path(&self.workspace_root, &self.current_id(), number);
        if let Ok(state) = self.agent.session().export_state() {
            let _ = dino_core::resume::save(&path, &state, None);
        }
    }

    pub(crate) fn checkpoints(&self) -> String {
        match dino_core::checkpoint::list(&self.workspace_root, &self.current_id()) {
            Ok(checkpoints) => listing(&checkpoints, unix_now()),
            Err(error) => style::paint(style::WARN, &format!("  {error}\n")),
        }
    }

    /// Says what going back to before `number` would do, and asks how.
    pub(crate) fn preview_rewind(&mut self, number: &str) {
        let id = self.current_id();
        let found = number.parse::<u32>().ok().and_then(|number| {
            dino_core::checkpoint::list(&self.workspace_root, &id)
                .ok()?
                .into_iter()
                .find(|checkpoint| checkpoint.number == number)
        });
        let Some(checkpoint) = found else {
            println!(
                "{}",
                style::paint(
                    style::WARN,
                    &format!("  there is no checkpoint {number}; /rewind lists them")
                )
            );
            return;
        };
        let plan = match dino_core::checkpoint::plan(&self.workspace_root, &id, checkpoint.number) {
            Ok(plan) => plan,
            Err(error) => {
                println!("{}", style::paint(style::WARN, &format!("  {error}")));
                return;
            }
        };
        let kept = dino_core::checkpoint::state_path(&self.workspace_root, &id, checkpoint.number)
            .is_file();
        print!("{}", preview(&checkpoint, &plan, kept, unix_now()));
        self.rewinding = Some(checkpoint.number);
    }

    /// Goes back to before checkpoint `number`. The conversation is restored
    /// first, so a state that will not load leaves the files alone too.
    pub(crate) fn rewind(&mut self, number: u32, files: bool, conversation: bool) {
        let root = self.workspace_root.clone();
        let id = self.current_id();
        let warn = |text: &str| println!("{}", style::paint(style::WARN, text));
        let restored = if conversation {
            let path = dino_core::checkpoint::state_path(&root, &id, number);
            let restored = dino_core::resume::load(&path)
                .map_err(anyhow::Error::from)
                .and_then(|state| {
                    let (mut session, name) = (self.factory)(&state.model_key)?;
                    session.restore_state(&state)?;
                    Ok((session, name, state.model_key))
                });
            match restored {
                Ok(restored) => Some(restored),
                Err(error) => {
                    warn(&format!(
                        "  the conversation before {number} cannot be restored, so nothing \
                         was rewound: {error}"
                    ));
                    return;
                }
            }
        } else {
            None
        };
        let rewound = if files {
            match dino_core::checkpoint::rewind_files(&root, &id, number) {
                Ok(rewound) => Some(rewound),
                Err(error) => {
                    warn(&format!("  nothing was rewound: {error}"));
                    return;
                }
            }
        } else {
            None
        };
        if let Some((session, name, key)) = restored {
            let prompt = dino_core::checkpoint::list(&root, &id)
                .ok()
                .and_then(|list| list.into_iter().find(|c| c.number == number))
                .map(|checkpoint| checkpoint.prompt);
            if let Err(error) = dino_core::checkpoint::truncate(&root, &id, number) {
                warn(&format!("  {error}"));
            }
            self.agent.replace_session(session);
            self.recall = crate::recall::Recall::default();
            self.model_key = key;
            self.model = name;
            if let Ok(state) = self.agent.session().export_state()
                && let Err(error) =
                    dino_core::resume::save(&self.state_path, &state, self.name.as_deref())
            {
                warn(&format!("  could not save the session: {error}"));
            }
            println!(
                "{}",
                style::paint(
                    FAINT,
                    &format!(
                        "  the conversation is back to before {number}; that message is ready to edit"
                    )
                )
            );
            self.typed_ahead = prompt.unwrap_or_default();
        }
        if let Some(rewound) = rewound {
            print!("{}", report(&rewound));
            // Recorded as a checkpoint of its own, so going back to it undoes it.
            let at = if conversation {
                Ok(number)
            } else {
                dino_core::checkpoint::next(&root, &id)
            };
            let recorded = at.and_then(|at| {
                let note = format!("(the files rewound to before {number})");
                dino_core::checkpoint::record_rewind(&root, &id, at, &note, &rewound)?;
                self.keep_conversation(at);
                Ok(at)
            });
            match recorded {
                Ok(at) => println!(
                    "{}",
                    style::paint(FAINT, &format!("  /rewind {at} undoes this"))
                ),
                Err(error) => warn(&format!("  the rewind is not undoable: {error}")),
            }
        }
    }

    /// Carries on in a copy of this conversation, with its checkpoints, and
    /// leaves the original as it is.
    pub(crate) fn fork(&mut self, name: Option<String>) {
        let state = match self.agent.session().export_state() {
            Ok(state) if self.state_path.is_file() => state,
            Ok(_) => {
                println!("{}", style::paint(FAINT, "  nothing to fork yet"));
                return;
            }
            Err(error) => {
                println!("{}", style::paint(style::WARN, &format!("  {error}")));
                return;
            }
        };
        let parent = self.current_id();
        let child = format!("{}-{}", self.run_id, self.conversations + 1);
        match dino_core::resume::fork(
            &self.workspace_root,
            &parent,
            &child,
            &state,
            name.as_deref(),
        ) {
            Ok(_) => {
                self.conversations += 1;
                self.state_path = dino_core::resume::state_path(&self.workspace_root, &child);
                self.name = name;
                self.note_conversation();
                println!(
                    "{}",
                    style::paint(
                        FAINT,
                        &format!("  forked {parent} as {child}; /resume {parent} goes back to it")
                    )
                );
            }
            Err(error) => println!("{}", style::paint(style::WARN, &format!("  {error}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_listing_shows_the_latest_and_the_first_line_of_each() {
        let checkpoints: Vec<Checkpoint> = (1..=25)
            .map(|number| Checkpoint {
                number,
                unix_seconds: 1_000,
                prompt: format!("message {number}\nand more"),
            })
            .collect();
        let listed = listing(&checkpoints, 1_090);
        assert!(!listed.contains("message 5\n"), "{listed}");
        assert!(listed.contains("message 6"), "{listed}");
        assert!(listed.contains("message 25"), "{listed}");
        assert!(!listed.contains("and more"), "{listed}");
        assert!(listed.contains("1m ago"), "{listed}");
    }
}
