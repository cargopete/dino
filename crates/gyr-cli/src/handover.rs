//! `gyr run --handover`: a long task split across fresh sessions, carried by a
//! progress record. RFC-0018.

use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use gyr_core::AgentError;
use gyr_core::FinishCheck as _;
use gyr_core::progress::Progress;
use gyr_core::progress::Record;
use gyr_protocol::AgentEvent;
use gyr_protocol::StopReason;
use gyr_sandbox::Sandbox;
use tokio_util::sync::CancellationToken;

use crate::Prepared;
use crate::config;

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
}

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
        let finished = matches!(stop, Some(StopReason::Cancelled | StopReason::Blocked))
            || record.items.is_empty()
            || record.pending() == 0;
        if finished || session >= MAX_SESSIONS || used >= total {
            return Ok(stop.map_or(Ended::OutOfTurns, Ended::Stopped));
        }
        // A session that ended of its own accord without finishing an item
        // is allowed once; twice in a row ends the chain rather than looping.
        if stop.is_some() && record.done() == last_done {
            if stalled {
                return Ok(Ended::Stopped(StopReason::EndTurn));
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
        let fresh = config::build_session(
            &prepared.settings,
            prepared.system_prompt.clone(),
            prepared.definitions.clone(),
        )?;
        prepared.agent.replace_session(fresh);
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
    if let Some(git) = gyr_core::git::GitSnapshot::take(workspace)
        && git.status_total > 0
    {
        let _ = write!(
            &mut text,
            "\nChanged files ({}):\n{}\n",
            git.status_total,
            git.status.join("\n")
        );
    }
    let build = match gyr_eval::build_check(workspace, Arc::clone(sandbox)) {
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
