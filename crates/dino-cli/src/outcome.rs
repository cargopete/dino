//! `dino run --result`: how a run ended, in one JSON object for a script, so
//! it need not cut the final message out of the transcript.

use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use dino_core::AgentError;
use dino_core::RunResult;
use dino_protocol::AgentEvent;
use dino_protocol::StopReason;
use dino_protocol::TokenUsage;
use serde::Serialize;

use crate::handover::Ended;
use crate::spend::Spent;

#[derive(Debug, Serialize, PartialEq)]
pub struct Outcome {
    /// `finished`, `stopped`, `blocked`, `unfinished`, `cancelled` or `failed`.
    pub status: &'static str,
    pub exit_code: u8,
    pub stop_reason: Option<StopReason>,
    /// Why a limit or a failure ended the run, when one did.
    pub reason: Option<String>,
    /// The model's last message. `None` for a `--handover` chain or a failure.
    pub message: Option<String>,
    pub model_turns: Option<u32>,
    /// Calls the approval policy refused, which a run can end well despite.
    pub refused_calls: Option<usize>,
    pub tokens: TokenUsage,
    pub cost_list_usd: f64,
    /// Responses from a model without a list price, which the cost leaves out.
    pub unpriced_responses: u64,
    pub session_id: String,
    pub log: String,
}

/// The exit code a run's stop reason gives.
#[must_use]
pub fn exit_code(reason: StopReason) -> u8 {
    match reason {
        StopReason::EndTurn => 0,
        StopReason::Cancelled => 130,
        _ => 1,
    }
}

impl Outcome {
    /// One session's run, after any wrap-up a limit asked for.
    #[must_use]
    pub fn of_run(
        result: &Result<RunResult, AgentError>,
        stopped: Option<&str>,
        spent: Spent,
        session_id: &str,
        log: &Path,
    ) -> Self {
        let mut outcome = Self::blank(spent, session_id, log);
        match result {
            Ok(run) => {
                outcome.exit_code = if stopped.is_some() {
                    1
                } else {
                    exit_code(run.stop_reason)
                };
                outcome.status = match (stopped, run.stop_reason) {
                    (Some(_), _) => "stopped",
                    (None, StopReason::EndTurn) => "finished",
                    (None, StopReason::Cancelled) => "cancelled",
                    (None, StopReason::Blocked) => "blocked",
                    (None, _) => "unfinished",
                };
                outcome.stop_reason = Some(run.stop_reason);
                outcome.reason = stopped.map(str::to_owned);
                outcome.message = Some(run.text.clone());
                outcome.model_turns = Some(run.model_turns);
                outcome.refused_calls = Some(refused(&run.events));
            }
            Err(error) => {
                if stopped.is_some() {
                    outcome.status = "stopped";
                }
                outcome.reason = Some(stopped.map_or_else(|| error.to_string(), str::to_owned));
            }
        }
        outcome
    }

    /// A `--handover` chain, which keeps no one final message.
    #[must_use]
    pub fn of_chain(ended: &Result<Ended>, spent: Spent, session_id: &str, log: &Path) -> Self {
        let mut outcome = Self::blank(spent, session_id, log);
        let (status, exit, stop, reason) = match ended {
            Ok(Ended::Stopped(StopReason::EndTurn)) => {
                ("finished", 0, Some(StopReason::EndTurn), None)
            }
            Ok(Ended::Stopped(StopReason::Cancelled)) => {
                ("cancelled", 130, Some(StopReason::Cancelled), None)
            }
            Ok(Ended::Stopped(StopReason::Blocked)) => {
                ("blocked", 1, Some(StopReason::Blocked), None)
            }
            Ok(Ended::Stopped(other)) => ("unfinished", 1, Some(*other), None),
            Ok(Ended::OutOfTurns) => ("unfinished", 1, None, Some("out of turns".to_owned())),
            Ok(Ended::Stalled) => (
                "unfinished",
                1,
                None,
                Some("two sessions in a row ended with the plan unfinished".to_owned()),
            ),
            Ok(Ended::OverBudget(reason)) => ("stopped", 1, None, Some(reason.clone())),
            Err(error) => ("failed", 1, None, Some(error.to_string())),
        };
        outcome.status = status;
        outcome.exit_code = exit;
        outcome.stop_reason = stop;
        outcome.reason = reason;
        outcome
    }

    fn blank(spent: Spent, session_id: &str, log: &Path) -> Self {
        Self {
            status: "failed",
            exit_code: 1,
            stop_reason: None,
            reason: None,
            message: None,
            model_turns: None,
            refused_calls: None,
            tokens: spent.tokens,
            cost_list_usd: spent.usd,
            unpriced_responses: spent.unpriced,
            session_id: session_id.to_owned(),
            log: log.display().to_string(),
        }
    }

    /// # Errors
    ///
    /// Returns an error naming the path when it cannot be written.
    pub fn write(&self, path: &Path) -> Result<()> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
    }
}

fn refused(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(event, AgentEvent::ToolDecided { decision, .. } if !decision.is_allowed())
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spent() -> Spent {
        Spent {
            usd: 1.25,
            unpriced: 0,
            tokens: TokenUsage {
                input_tokens: 1_000,
                output_tokens: 200,
                ..TokenUsage::default()
            },
            last_usd: 0.25,
        }
    }

    fn run(reason: StopReason, text: &str) -> RunResult {
        RunResult {
            events: Vec::new(),
            text: text.to_owned(),
            stop_reason: reason,
            model_turns: 3,
        }
    }

    #[test]
    fn a_finished_run_carries_its_last_message_and_cost() {
        let outcome = Outcome::of_run(
            &Ok(run(StopReason::EndTurn, "done: tests pass")),
            None,
            spent(),
            "s1",
            Path::new("run.jsonl"),
        );
        assert_eq!(outcome.status, "finished");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.message.as_deref(), Some("done: tests pass"));
        assert_eq!(outcome.model_turns, Some(3));
        assert_eq!(outcome.refused_calls, Some(0));
        assert_eq!(outcome.tokens.input_tokens, 1_000);
        assert!((outcome.cost_list_usd - 1.25).abs() < f64::EPSILON);
    }

    #[test]
    fn a_limit_stops_with_exit_one_even_after_a_wrap_up_that_ended_its_turn() {
        let outcome = Outcome::of_run(
            &Ok(run(StopReason::EndTurn, "half done")),
            Some("the task has spent $3.00 of its $3.00 budget at list price"),
            spent(),
            "s1",
            Path::new("run.jsonl"),
        );
        assert_eq!((outcome.status, outcome.exit_code), ("stopped", 1));
        assert!(outcome.reason.unwrap().contains("$3.00"));
    }

    #[test]
    fn a_failed_chain_says_why() {
        let outcome = Outcome::of_chain(
            &Err(anyhow::anyhow!("model transport failed")),
            spent(),
            "s1",
            Path::new("run.jsonl"),
        );
        assert_eq!((outcome.status, outcome.exit_code), ("failed", 1));
        assert_eq!(outcome.reason.as_deref(), Some("model transport failed"));
        assert_eq!(outcome.message, None);
    }
}
