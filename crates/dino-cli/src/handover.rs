//! `dino run --handover`: a long task split across fresh sessions, carried by a
//! progress record. RFC-0018.

use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use dino_core::AgentError;
use dino_core::FinishCheck as _;
use dino_core::progress::Progress;
use dino_core::progress::Record;
use dino_protocol::AgentEvent;
use dino_protocol::StopReason;
use dino_sandbox::Sandbox;
use tokio_util::sync::CancellationToken;

use crate::Prepared;

/// Sessions a chain may run, the first included.
const MAX_SESSIONS: u32 = 4;

/// Added to the task for the first session only, so the system prompt stays
/// byte-identical across the chain and the provider's prefix cache holds.
const BRIEF: &str = "This task may continue in fresh sessions that see only the workspace \
    and a progress record. Before changing anything, record a plan of 2 to 6 items with the \
    progress tool. Mark each item done as soon as it is finished and verified, and use op note \
    for what a fresh session would need: what you tried, why it failed, where things are.";

/// How a chain ended, for the exit code.
pub enum Ended {
    Stopped(StopReason),
    OutOfTurns,
    /// Sessions kept ending with items pending and nothing newly done.
    Stalled,
}

/// Sent once, in the same session, when it ends its turn with items pending:
/// models finish the work and forget the record more often than they stop
/// short, and a fresh session costs far more than one question.
const PENDING: &str = "The progress record still has pending items. If they are finished and \
    verified, mark them with the progress tool, op done, several at once if you like. If not, \
    carry on with the next one.";

/// Runs the task, handing over to a fresh session while items stay pending.
///
/// # Errors
///
/// Returns an error when a run fails for any reason other than reaching its
/// turn limit, or when a fresh session cannot be built.
pub async fn run(
    prepared: &mut Prepared,
    progress: &Progress,
    task: &str,
    per_session: u32,
    cancel: &CancellationToken,
) -> Result<Ended> {
    let total = prepared.settings.max_turns.get();
    let mut used = 0;
    let mut session = 1;
    let mut last_done = 0;
    let mut stalled = false;
    let mut asked = false;
    let mut message = format!("{task}\n\n{BRIEF}");
    loop {
        let turns = per_session.min(total.saturating_sub(used)).max(1);
        prepared
            .agent
            .set_max_model_turns(NonZeroU32::new(turns).unwrap_or(NonZeroU32::MIN));
        let outcome = prepared.agent.run_cancellable(message, cancel).await;
        crate::save_state(&prepared.agent, &prepared.state_path)?;
        let (reason, stop) = match outcome {
            Ok(result) => {
                used += result.model_turns;
                (ended_early(), Some(result.stop_reason))
            }
            Err(AgentError::ModelTurnLimit(_)) => {
                used += turns;
                ("it reached its turn limit".to_owned(), None)
            }
            Err(error) => return Err(error.into()),
        };

        let record = progress.snapshot();
        // Every item done is a finished task, even when the session spent its
        // last turn saying so.
        let complete = !record.items.is_empty() && record.done() == record.items.len();
        if complete && !matches!(stop, Some(StopReason::Cancelled | StopReason::Blocked)) {
            if stop.is_none() {
                eprintln!(
                    "{}",
                    crate::style::paint(
                        crate::style::FAINT,
                        "  every item in the plan is done; the run is finished"
                    )
                );
            }
            return Ok(Ended::Stopped(StopReason::EndTurn));
        }
        let finished = matches!(stop, Some(StopReason::Cancelled | StopReason::Blocked))
            || record.items.is_empty()
            || record.pending() == 0;
        if !finished && stop == Some(StopReason::EndTurn) && !asked && used < total {
            asked = true;
            message = String::from(PENDING);
            continue;
        }
        if finished {
            // Nothing pending but not all done: some items were marked blocked.
            if !record.items.is_empty() && record.done() < record.items.len() {
                return Ok(Ended::Stopped(StopReason::Blocked));
            }
            return Ok(stop.map_or(Ended::OutOfTurns, Ended::Stopped));
        }
        // Out of sessions or turns with the plan unfinished is unfinished,
        // whatever the last session's own stop reason said.
        if session >= MAX_SESSIONS || used >= total {
            return Ok(Ended::OutOfTurns);
        }
        // A session that ended of its own accord without finishing an item
        // is allowed once; twice in a row ends the chain rather than looping.
        if stop.is_some() && record.done() == last_done {
            if stalled {
                return Ok(Ended::Stalled);
            }
            stalled = true;
        } else {
            stalled = false;
        }
        last_done = record.done();

        session += 1;
        prepared.agent.record(&AgentEvent::HandedOver {
            session,
            reason: reason.clone(),
            pending: record.pending(),
        })?;
        let fresh = prepared.build_session(&prepared.settings)?;
        prepared.agent.replace_session(fresh);
        asked = false;
        message = seed(
            task,
            &record,
            &reason,
            &prepared.settings.workspace,
            &prepared.sandbox,
        )
        .await;
    }
}

fn ended_early() -> String {
    "it ended its turn with items still pending".to_owned()
}

/// The first message of a fresh session, built by the harness: the task, the
/// record, why the last session ended, what has changed, and the build.
async fn seed(
    task: &str,
    record: &Record,
    reason: &str,
    workspace: &Path,
    sandbox: &Arc<dyn Sandbox>,
) -> String {
    let mut text = format!(
        "{task}\n\nThis continues the task in a fresh session; the last one ended because \
         {reason}. Only the workspace and this record carried over.\n\nProgress record:\n{}",
        record.render()
    );
    if let Some(git) = dino_core::git::GitSnapshot::take(workspace)
        && git.status_total > 0
    {
        let _ = write!(
            &mut text,
            "\nChanged files ({}):\n{}\n",
            git.status_total,
            git.status.join("\n")
        );
    }
    let build = match dino_eval::build_check(workspace, Arc::clone(sandbox)) {
        Ok(Some(check)) => check
            .unfinished()
            .await
            .unwrap_or_else(|| "`cargo check` is clean.".to_owned()),
        _ => "No Cargo build here to check.".to_owned(),
    };
    let _ = write!(
        &mut text,
        "\nBuild: {build}\n\nCarry on with the next pending item, and keep the record current \
         with the progress tool."
    );
    text
}
