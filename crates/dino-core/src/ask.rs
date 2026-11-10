//! A way for the model to put a decision to the person in the middle of a
//! turn, as a few labelled options, and carry on with the answer.
//!
//! Without one, a model that needs a choice ends its turn with the question in
//! prose and waits for a typed reply. A run nobody is watching must never wait,
//! so there the tool answers at once that no one is there.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use crate::ToolError;
use crate::ToolFuture;
use crate::ToolRuntime;

pub const ASK: &str = "ask";
pub const MAX_QUESTIONS: usize = 4;
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 4;
/// What the model is told when no one can answer.
pub const NO_ONE: &str = "no one to ask; take the recommended option and say so in the summary";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub question: String,
    /// The first is the one the model recommends.
    pub options: Vec<Choice>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One question's answer: the labels picked, or the person's own words.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub question: String,
    pub chosen: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

/// Every answer, and where they were given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answered {
    /// `terminal` or `page`.
    pub by: String,
    pub answers: Vec<Answer>,
}

pub type AnswerFuture<'a> = Pin<Box<dyn Future<Output = Option<Answered>> + Send + 'a>>;

/// A frontend that can put questions to a person. `None` when nobody could
/// be asked, such as a closed terminal.
pub trait Asker: Send + Sync {
    fn ask(&self, questions: Vec<Question>) -> AnswerFuture<'_>;
}

/// Asks no one.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOne;

impl Asker for NoOne {
    fn ask(&self, _questions: Vec<Question>) -> AnswerFuture<'_> {
        Box::pin(async { None })
    }
}

pub struct AskTool {
    asker: Arc<dyn Asker>,
    /// Cleared while the session sees every submission through unasked.
    attended: Arc<AtomicBool>,
}

impl AskTool {
    pub fn new(asker: Arc<dyn Asker>, attended: Arc<AtomicBool>) -> Self {
        Self { asker, attended }
    }

    /// For a run with nobody to answer, such as `dino run`.
    #[must_use]
    pub fn unattended() -> Self {
        Self::new(Arc::new(NoOne), Arc::new(AtomicBool::new(false)))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskArguments {
    questions: Vec<Question>,
}

/// The questions of an `ask` call, checked.
///
/// # Errors
///
/// Returns [`ToolError`] when the arguments do not parse, or ask none or too
/// many questions, or a question has too few or too many options.
pub fn questions(call: &ToolCall) -> Result<Vec<Question>, ToolError> {
    if call.name != ASK {
        return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
    }
    let arguments: AskArguments = serde_json::from_value(call.arguments.clone())
        .map_err(|error| ToolError::new(format!("invalid {ASK} arguments: {error}")))?;
    let count = arguments.questions.len();
    if count == 0 || count > MAX_QUESTIONS {
        return Err(ToolError::new(format!(
            "ask one to {MAX_QUESTIONS} questions, not {count}"
        )));
    }
    for question in &arguments.questions {
        if question.question.trim().is_empty() {
            return Err(ToolError::new("a question is empty"));
        }
        let options = question.options.len();
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&options) {
            return Err(ToolError::new(format!(
                "{:?} has {options} options; give {MIN_OPTIONS} to {MAX_OPTIONS}",
                question.question
            )));
        }
        let mut labels: Vec<&str> = question
            .options
            .iter()
            .map(|choice| choice.label.trim())
            .collect();
        if labels.iter().any(|label| label.is_empty()) {
            return Err(ToolError::new("an option has no label"));
        }
        labels.sort_unstable();
        labels.dedup();
        if labels.len() != options {
            return Err(ToolError::new(format!(
                "{:?} names an option twice",
                question.question
            )));
        }
    }
    Ok(arguments.questions)
}

impl ToolRuntime for AskTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: ASK.into(),
            description: "Ask the person to decide something you cannot settle from the code, \
                          the task or the conventions, and carry on with the answer instead of \
                          ending your turn to ask in prose. Up to four questions, each with two \
                          to four short options; put the one you recommend first. Set \
                          multi_select where several can apply. The person may also write an \
                          answer of their own. If no one is there, the result says so: take \
                          the recommended option and say so in your summary."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_QUESTIONS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string"},
                                "options": {
                                    "type": "array",
                                    "minItems": MIN_OPTIONS,
                                    "maxItems": MAX_OPTIONS,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string"},
                                            "description": {"type": "string"}
                                        },
                                        "required": ["label"],
                                        "additionalProperties": false
                                    }
                                },
                                "multi_select": {"type": "boolean"}
                            },
                            "required": ["question", "options"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        questions(call)?;
        Ok(ToolAction::read_only())
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let asked = questions(call);
        Box::pin(async move {
            let asked = asked?;
            if !self.attended.load(Ordering::Relaxed) {
                return Ok(ToolOutput::success(NO_ONE));
            }
            match self.asker.ask(asked).await {
                Some(answered) => serde_json::to_string(&answered)
                    .map(ToolOutput::success)
                    .map_err(|error| ToolError::new(format!("cannot encode the answers: {error}"))),
                None => Ok(ToolOutput::success(NO_ONE)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: ASK.into(),
            arguments,
        }
    }

    fn one_question() -> serde_json::Value {
        json!({"questions": [{
            "question": "Which error type?",
            "options": [{"label": "thiserror"}, {"label": "anyhow", "description": "for a binary"}]
        }]})
    }

    struct Picks(Answered);

    impl Asker for Picks {
        fn ask(&self, _questions: Vec<Question>) -> AnswerFuture<'_> {
            let answered = self.0.clone();
            Box::pin(async move { Some(answered) })
        }
    }

    #[tokio::test]
    async fn an_answer_comes_back_as_the_result() {
        let answered = Answered {
            by: "terminal".into(),
            answers: vec![Answer {
                question: "Which error type?".into(),
                chosen: vec!["anyhow".into()],
                other: None,
            }],
        };
        let tool = AskTool::new(
            Arc::new(Picks(answered.clone())),
            Arc::new(AtomicBool::new(true)),
        );
        let output = tool.execute(&call(one_question())).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Answered>(&output.content).unwrap(),
            answered
        );
    }

    #[tokio::test]
    async fn an_unattended_run_is_told_no_one_is_there_without_waiting() {
        let tool = AskTool::unattended();
        let output = tool.execute(&call(one_question())).await.unwrap();
        assert_eq!(output.content, NO_ONE);
        assert!(!output.is_error);
    }

    #[test]
    fn questions_are_bounded_and_their_options_distinct() {
        let tool = AskTool::unattended();
        assert_eq!(
            tool.classify(&call(one_question())).unwrap(),
            ToolAction::read_only()
        );
        let option = json!({"label": "a"});
        let question =
            |options: Vec<serde_json::Value>| json!({"question": "q", "options": options});
        for arguments in [
            json!({"questions": []}),
            json!({"questions": vec![question(vec![option.clone(), json!({"label": "b"})]); 5]}),
            json!({"questions": [question(vec![option.clone()])]}),
            json!({"questions": [question(vec![option.clone(), option.clone()])]}),
        ] {
            assert!(
                tool.classify(&call(arguments.clone())).is_err(),
                "{arguments}"
            );
        }
    }
}
