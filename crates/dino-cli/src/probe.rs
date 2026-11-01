//! The `dino probe` command.
//!
//! Nine checks against one endpoint, the last of which, the costly context
//! window, can be skipped, as can the four awkward histories before it. A
//! failure here is a report, not a gate on `dino run`. Each check opens its own
//! session, so history cannot leak between them. Thinking on and thinking off
//! are two sessions: the switch is fixed when the session is built.
//!
//! The histories are seeded the way a fallback hands a conversation over, so
//! they test that path too. Dino sends a note after tool results every turn, so
//! that one must be answered. The other three it never means to send; what it
//! needs from a provider sent one is a verdict, an answer or a refusal of the
//! request, rather than a stream that breaks. Which of the two is evidence.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use dino_model::ModelError;
use dino_model::ModelSession;
use dino_model::SessionState;
use dino_protocol::ModelEvent;
use dino_protocol::ModelProfile;
use dino_protocol::ProviderKind;
use dino_protocol::ReasoningSupport;
use dino_protocol::TokenUsage;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_protocol::ToolResult;
use dino_protocol::TurnInput;
use futures_util::StreamExt;
use serde::Serialize;

use crate::config;
use crate::config::RunSettings;
use crate::style;
use crate::style::FAINT;
use crate::style::OK;
use crate::style::PURPLE;
use crate::style::WARN;

const HEAD: &str = "DINOPROBEHEAD";
const TAIL: &str = "DINOPROBETAIL";
const SCHEMA: &str = "DINOPROBESCHEMA";
const TOKEN: &str = "DINOPROBETOKEN";
const WINDOW: &str = "DINOPROBEWINDOW";
const NOTE: &str = "DINOPROBENOTE";

const TOOL_PHRASE: &str = "Call the tool probe_echo";
const PLAIN_SYSTEM: &str = "Reply briefly.";
const CONTEXT_SYSTEM: &str = "Repeat the code on the last line of the user message.";
const TOOL_USER: &str = "Call the tool now.";
const PROMPT_USER: &str = "Reply with the line just above the instruction in the system prompt, \
     the line just below it, and the sentinel from the tool description.";
const THINK_USER: &str = "Think step by step, then reply with ok.";
const WARMUP_USER: &str = "Reply with the word ok.";

const TOOL_PARSER: &str = "tool_parser";
const PROMPT_AND_SCHEMA: &str = "prompt_and_schema";
const THINKING: &str = "thinking";
const THROUGHPUT: &str = "throughput";
const CONTEXT_WINDOW: &str = "context_window";
const NOTE_AFTER_RESULTS: &str = "note_after_results";
const ORPHANED_RESULT: &str = "orphaned_result";
const DUPLICATE_CALL_ID: &str = "duplicate_call_id";
const RESULTS_OUT_OF_ORDER: &str = "results_out_of_order";

/// Tokens held back from the declared window so the reply still fits.
const MARGIN: u32 = 4096;
const TRIALS: usize = 3;
const PREFILL_TOKENS: u32 = 256;
// Completions, not the thinking budget. A ramble is not the measurement.
const MAX_OUTPUT_TOKENS: u32 = 256;
// The tool and prompt checks are read from the answer, which comes after the
// reasoning, and Moonshot counts reasoning against the cap. Kimi spent all of
// 256 thinking about the prompt check and up to 521 when given room.
const ANSWER_TOKENS: u32 = 4096;
// A history is a few short messages; the cap leaves a reasoning model room to
// reach its answer.
const HISTORY_TOKENS: u32 = 512;

// Command-line flags are booleans by nature.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct ProbeArgs {
    /// Model key from `dino models`. Flag, then `DINO_MODEL`, then the config file.
    #[arg(long)]
    pub model: Option<String>,
    /// Workspace root. Defaults to the current directory.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Endpoint for a self-served model, standing in for `QWEN_API_BASE`.
    #[arg(long)]
    pub api_base: Option<String>,
    /// Canary length in tokens. Defaults to 4096 below the served window, or
    /// the declared one when the server does not say. That default is a large
    /// request.
    #[arg(long)]
    pub canary_tokens: Option<u32>,
    /// Skip the context-window check, which sends a canary as long as the
    /// window: about a million tokens, and a few dollars, on Kimi.
    #[arg(long, conflicts_with = "canary_tokens")]
    pub skip_context_window: bool,
    /// Skip the four awkward histories: a note after tool results, an orphaned
    /// result, a reused call id and results out of order. Each is one small
    /// request.
    #[arg(long)]
    pub skip_history: bool,
    /// Emit the report as JSON.
    #[arg(long)]
    pub json: bool,
    /// Never emit terminal colour.
    #[arg(long)]
    pub plain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Thinking {
    Default,
    On,
    Off,
}

struct SessionRequest {
    system: String,
    tools: Vec<ToolDefinition>,
    thinking: Thinking,
    max_output_tokens: u32,
}

trait ProbeHost {
    /// Opens one fresh session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session cannot be built, for example a missing
    /// credential. That aborts the command. A later transport failure does not:
    /// it fails the check that hit it.
    fn open(&mut self, request: SessionRequest) -> Result<Box<dyn ModelSession>>;
}

struct ProbeOptions {
    model: String,
    catalogue_window: Option<u32>,
    reasoning: ReasoningSupport,
    canary_tokens: Option<u32>,
    context_window: bool,
    history: bool,
    /// Prints the canary size on stderr. Tests leave this off.
    announce: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Check {
    name: &'static str,
    passed: bool,
    evidence: String,
}

#[derive(Debug, Serialize)]
struct ProbeReport {
    model: String,
    passed: bool,
    checks: Vec<Check>,
}

#[derive(Debug, Default)]
struct Transcript {
    text: String,
    reasoning: String,
    calls: Vec<ToolCall>,
    usage: Option<TokenUsage>,
    ttft: Option<Duration>,
    elapsed: Duration,
}

enum TurnOutcome {
    Done(Transcript),
    Failed(String),
}

/// Runs the checks and prints the report.
///
/// # Errors
///
/// Returns an error when a session cannot be opened, for example a missing
/// credential. A check that fails is a report and an exit code, not an error.
pub(crate) async fn execute(mut settings: RunSettings, args: ProbeArgs) -> Result<ExitCode> {
    // A probe measures one model; a fallback would answer for it.
    settings.fallback.clear();
    style::enable(args.plain);
    let mut options = options_from(&settings.profile, args.canary_tokens, true);
    options.context_window = !args.skip_context_window;
    options.history = !args.skip_history;
    let report = run_probe(&mut LiveHost { settings }, &options).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    Ok(if report.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn options_from(
    profile: &ModelProfile,
    canary_tokens: Option<u32>,
    announce: bool,
) -> ProbeOptions {
    ProbeOptions {
        model: profile.key.clone(),
        catalogue_window: profile.context_window_tokens,
        reasoning: profile.reasoning.clone(),
        canary_tokens,
        context_window: true,
        history: true,
        announce,
    }
}

struct LiveHost {
    settings: RunSettings,
}

impl ProbeHost for LiveHost {
    fn open(&mut self, request: SessionRequest) -> Result<Box<dyn ModelSession>> {
        let mut settings = self.settings.clone();
        settings.profile.max_output_tokens = Some(request.max_output_tokens);
        // The probe sets the boolean itself. A configured no-thinking must not stick.
        settings.disable_thinking = false;
        settings.enable_thinking = match request.thinking {
            Thinking::Default => None,
            Thinking::On => Some(true),
            Thinking::Off => Some(false),
        };
        config::build_session(&settings, request.system, request.tools)
    }
}

async fn run_probe(host: &mut impl ProbeHost, options: &ProbeOptions) -> Result<ProbeReport> {
    let mut checks = vec![
        check_tool_parser(host).await?,
        check_prompt(host).await?,
        check_thinking(host, &options.reasoning).await?,
        check_throughput(host).await?,
    ];
    if options.history {
        for history in histories() {
            checks.push(check_history(host, history).await?);
        }
    }
    if options.context_window {
        checks.push(check_context(host, options).await?);
    }
    let passed = checks.iter().all(|check| check.passed);
    Ok(ProbeReport {
        model: options.model.clone(),
        passed,
        checks,
    })
}

async fn check_tool_parser(host: &mut impl ProbeHost) -> Result<Check> {
    let outcome = turn(
        host,
        SessionRequest {
            system: tool_system(),
            tools: vec![probe_tool()],
            thinking: Thinking::Default,
            max_output_tokens: ANSWER_TOKENS,
        },
        TOOL_USER.to_owned(),
    )
    .await?;
    let transcript = match outcome {
        TurnOutcome::Done(transcript) => transcript,
        TurnOutcome::Failed(error) => return Ok(failed(TOOL_PARSER, error)),
    };
    let structured = transcript
        .calls
        .iter()
        .any(|call| call.name == "probe_echo" && call.arguments.to_string().contains(TOKEN));
    if structured {
        return Ok(passed(
            TOOL_PARSER,
            format!("structured call carried {TOKEN}"),
        ));
    }
    if transcript.text.contains(TOKEN) {
        return Ok(failed(
            TOOL_PARSER,
            format!("{TOKEN} arrived as text, not as a tool call"),
        ));
    }
    Ok(failed(
        TOOL_PARSER,
        "no structured call carried the sentinel",
    ))
}

async fn check_prompt(host: &mut impl ProbeHost) -> Result<Check> {
    let outcome = turn(
        host,
        SessionRequest {
            system: prompt_system(),
            tools: vec![probe_tool()],
            thinking: Thinking::Default,
            max_output_tokens: ANSWER_TOKENS,
        },
        PROMPT_USER.to_owned(),
    )
    .await?;
    let transcript = match outcome {
        TurnOutcome::Done(transcript) => transcript,
        TurnOutcome::Failed(error) => return Ok(failed(PROMPT_AND_SCHEMA, error)),
    };
    let (ok, evidence) = prompt_evidence(&echoed(&transcript));
    Ok(if ok {
        passed(PROMPT_AND_SCHEMA, evidence)
    } else {
        failed(PROMPT_AND_SCHEMA, evidence)
    })
}

async fn check_thinking(host: &mut impl ProbeHost, reasoning: &ReasoningSupport) -> Result<Check> {
    let evidence = match reasoning {
        ReasoningSupport::None => "profile has no thinking control",
        ReasoningSupport::Effort(_) => "effort is not a boolean this probe flips",
        ReasoningSupport::ProviderManaged => {
            "provider-managed thinking is not a boolean this probe flips"
        }
        ReasoningSupport::Toggle => return check_toggle(host).await,
    };
    Ok(passed(THINKING, evidence))
}

async fn check_toggle(host: &mut impl ProbeHost) -> Result<Check> {
    let on = match sample_thinking(host, Thinking::On).await? {
        TurnOutcome::Failed(error) => return Ok(failed(THINKING, error)),
        TurnOutcome::Done(transcript) => saw_reasoning(&transcript),
    };
    let off = match sample_thinking(host, Thinking::Off).await? {
        TurnOutcome::Failed(error) => return Ok(failed(THINKING, error)),
        TurnOutcome::Done(transcript) => saw_reasoning(&transcript),
    };
    let (ok, evidence) = match (on, off) {
        (true, false) => (
            true,
            "thinking on produced reasoning, thinking off produced none",
        ),
        (true, true) => (
            false,
            "the control was ignored: thinking stayed on when asked off",
        ),
        (false, false) => (
            false,
            "the control was ignored: thinking on produced no reasoning",
        ),
        (false, true) => (
            false,
            "the control was ignored: thinking appeared only when asked off",
        ),
    };
    Ok(if ok {
        passed(THINKING, evidence)
    } else {
        failed(THINKING, evidence)
    })
}

async fn sample_thinking(host: &mut impl ProbeHost, thinking: Thinking) -> Result<TurnOutcome> {
    turn(host, plain_request(thinking), THINK_USER.to_owned()).await
}

async fn check_throughput(host: &mut impl ProbeHost) -> Result<Check> {
    if let TurnOutcome::Failed(error) = turn(
        host,
        plain_request(Thinking::Default),
        WARMUP_USER.to_owned(),
    )
    .await?
    {
        return Ok(failed(THROUGHPUT, error));
    }
    let mut parts = Vec::with_capacity(TRIALS);
    for trial in 1..=TRIALS {
        match turn(host, plain_request(Thinking::Default), prefill_message()).await? {
            TurnOutcome::Done(transcript) => {
                parts.push(format!("trial {trial}: {}", describe(&transcript)));
            }
            TurnOutcome::Failed(error) => {
                parts.push(format!("trial {trial}: {error}"));
                return Ok(failed(THROUGHPUT, parts.join("; ")));
            }
        }
    }
    Ok(passed(THROUGHPUT, parts.join("; ")))
}

/// A conversation so far, in the chat shape, and the tool results sent after it.
struct History {
    name: &'static str,
    prefix: serde_json::Value,
    results: &'static [&'static str],
    note: Option<String>,
    /// What the answer must hold; `None` when a refusal will do.
    answer_holds: Option<&'static str>,
}

fn histories() -> [History; 4] {
    let user = |content: &str| serde_json::json!({"role": "user", "content": content});
    let calls = |ids: &[&str]| {
        let calls: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": "probe_echo", "arguments": format!("{{\"token\":\"{TOKEN}\"}}")},
                })
            })
            .collect();
        serde_json::json!({"role": "assistant", "tool_calls": calls})
    };
    let result =
        |id: &str| serde_json::json!({"role": "tool", "tool_call_id": id, "content": "echoed"});
    [
        History {
            name: NOTE_AFTER_RESULTS,
            prefix: serde_json::json!([user("Call probe_echo."), calls(&["probe-1"])]),
            results: &["probe-1"],
            note: Some(format!("Reply with {NOTE}.")),
            answer_holds: Some(NOTE),
        },
        History {
            name: ORPHANED_RESULT,
            prefix: serde_json::json!([
                user("Reply with ok."),
                {"role": "assistant", "content": "ok"},
            ]),
            results: &["probe-ghost"],
            note: None,
            answer_holds: None,
        },
        History {
            name: DUPLICATE_CALL_ID,
            prefix: serde_json::json!([
                user("Call probe_echo twice, one call at a time."),
                calls(&["probe-1"]),
                result("probe-1"),
                calls(&["probe-1"]),
            ]),
            results: &["probe-1"],
            note: None,
            answer_holds: None,
        },
        History {
            name: RESULTS_OUT_OF_ORDER,
            prefix: serde_json::json!([
                user("Call probe_echo twice at once."),
                calls(&["probe-a", "probe-b"]),
            ]),
            results: &["probe-b", "probe-a"],
            note: None,
            answer_holds: None,
        },
    ]
}

async fn check_history(host: &mut impl ProbeHost, history: History) -> Result<Check> {
    let mut session = host.open(SessionRequest {
        system: PLAIN_SYSTEM.to_owned(),
        tools: vec![probe_tool()],
        thinking: Thinking::Default,
        max_output_tokens: HISTORY_TOKENS,
    })?;
    let name = history.name;
    if session.profile().provider == ProviderKind::OpenAi {
        return Ok(passed(
            name,
            "not applicable: OpenAI keeps the conversation on its server",
        ));
    }
    if let Err(error) = seed(&mut *session, history.prefix) {
        return Ok(failed(
            name,
            format!("the history could not be handed over: {error}"),
        ));
    }
    let input = TurnInput::ToolResults {
        results: history
            .results
            .iter()
            .map(|id| ToolResult {
                call_id: (*id).to_owned(),
                output: ToolOutput::success("echoed"),
            })
            .collect(),
        note: history.note,
    };
    Ok(
        match (attempt(&mut *session, input).await, history.answer_holds) {
            (Verdict::Answered(transcript), Some(needle)) if !transcript.text.contains(needle) => {
                failed(
                    name,
                    format!(
                        "answered without {needle}: {}",
                        excerpt(&echoed(&transcript))
                    ),
                )
            }
            (Verdict::Answered(transcript), _) => {
                passed(name, format!("answered: {}", excerpt(&echoed(&transcript))))
            }
            (Verdict::Refused(error), None) => passed(name, format!("refused: {error}")),
            (Verdict::Refused(error) | Verdict::Broken(error), _) => failed(name, error),
        },
    )
}

/// Puts `prefix` in the session as a fallback hands a conversation over.
fn seed(session: &mut dyn ModelSession, prefix: serde_json::Value) -> Result<(), ModelError> {
    let own = session.export_state()?;
    let chat = SessionState {
        provider: ProviderKind::Qwen,
        payload: prefix,
        ..own
    };
    let native = dino_model::fallback::hand_over(&chat, session.profile())?;
    session.restore_state(&native)
}

enum Verdict {
    Answered(Transcript),
    /// The provider turned the request down before anything streamed.
    Refused(String),
    Broken(String),
}

async fn attempt(session: &mut dyn ModelSession, input: TurnInput) -> Verdict {
    let started = Instant::now();
    let mut stream = match session.next(input).await {
        Ok(stream) => stream,
        Err(error) if error.rejects_request() => return Verdict::Refused(error.to_string()),
        Err(error) => return Verdict::Broken(error.to_string()),
    };
    let mut transcript = Transcript::default();
    let mut finished = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => {
                finished |= matches!(event, ModelEvent::Finished { .. });
                note(&mut transcript, event, started.elapsed());
            }
            Err(error) => return Verdict::Broken(error.to_string()),
        }
    }
    if finished {
        Verdict::Answered(transcript)
    } else {
        Verdict::Broken("the stream ended without finishing".to_owned())
    }
}

fn excerpt(text: &str) -> String {
    let text = text.trim();
    let cut: String = text.chars().take(80).collect();
    if cut.len() < text.len() {
        format!("{cut}…")
    } else {
        cut
    }
}

async fn check_context(host: &mut impl ProbeHost, options: &ProbeOptions) -> Result<Check> {
    let mut session = host.open(SessionRequest {
        system: CONTEXT_SYSTEM.to_owned(),
        tools: Vec::new(),
        thinking: Thinking::Default,
        max_output_tokens: MAX_OUTPUT_TOKENS,
    })?;
    // A server that will not say still leaves the catalogue window to measure
    // against. The error is evidence, and it does not abort the command.
    let (served, limits_error) = match session.discover_limits().await {
        Ok(()) => (session.profile().context_window_tokens, None),
        Err(error) => (None, Some(error.to_string())),
    };
    let tokens = match canary_len(options.catalogue_window, served, options.canary_tokens) {
        Ok(tokens) => tokens,
        Err(reason) => {
            let evidence = match &limits_error {
                Some(error) => format!("{error}; {reason}"),
                None => reason.to_owned(),
            };
            return Ok(failed(CONTEXT_WINDOW, evidence));
        }
    };
    if options.announce {
        eprintln!("context canary, about {tokens} tokens");
    }
    let transcript = match drive(
        &mut *session,
        TurnInput::User {
            content: canary_message(tokens),
        },
    )
    .await
    {
        Ok(transcript) => transcript,
        Err(error) => return Ok(failed(CONTEXT_WINDOW, error.to_string())),
    };
    let repeated = echoed(&transcript).contains(WINDOW);
    Ok(if repeated {
        passed(
            CONTEXT_WINDOW,
            context_evidence(
                options.catalogue_window,
                served,
                tokens,
                limits_error.as_deref(),
                &transcript,
                true,
            ),
        )
    } else {
        failed(
            CONTEXT_WINDOW,
            context_evidence(
                options.catalogue_window,
                served,
                tokens,
                limits_error.as_deref(),
                &transcript,
                false,
            ),
        )
    })
}

async fn turn(
    host: &mut impl ProbeHost,
    request: SessionRequest,
    user: String,
) -> Result<TurnOutcome> {
    let mut session = host.open(request)?;
    Ok(
        match drive(&mut *session, TurnInput::User { content: user }).await {
            Ok(transcript) => TurnOutcome::Done(transcript),
            Err(error) => TurnOutcome::Failed(error.to_string()),
        },
    )
}

async fn drive(
    session: &mut dyn ModelSession,
    input: TurnInput,
) -> std::result::Result<Transcript, ModelError> {
    let started = Instant::now();
    let mut stream = session.next(input).await?;
    let mut transcript = Transcript::default();
    while let Some(event) = stream.next().await {
        note(&mut transcript, event?, started.elapsed());
    }
    transcript.elapsed = started.elapsed();
    Ok(transcript)
}

fn note(transcript: &mut Transcript, event: ModelEvent, at: Duration) {
    match event {
        ModelEvent::TextDelta { text } => {
            mark_first_token(transcript, at);
            transcript.text.push_str(&text);
        }
        ModelEvent::ReasoningDelta { text } => {
            mark_first_token(transcript, at);
            transcript.reasoning.push_str(&text);
        }
        ModelEvent::ToolCallCompleted { call } => {
            mark_first_token(transcript, at);
            transcript.calls.push(call);
        }
        ModelEvent::Usage { usage } => {
            // Later usage events replace earlier ones. Summing would double-count.
            transcript.usage = Some(usage);
        }
        ModelEvent::Started { .. }
        | ModelEvent::ToolCallStarted { .. }
        | ModelEvent::ToolCallArgumentsDelta { .. }
        | ModelEvent::Finished { .. } => {}
    }
}

fn mark_first_token(transcript: &mut Transcript, at: Duration) {
    if transcript.ttft.is_none() {
        transcript.ttft = Some(at);
    }
}

fn saw_reasoning(transcript: &Transcript) -> bool {
    let reasoned = transcript
        .usage
        .is_some_and(|usage| usage.reasoning_tokens > 0);
    !transcript.reasoning.is_empty() || reasoned
}

fn echoed(transcript: &Transcript) -> String {
    let mut blob = transcript.text.clone();
    for call in &transcript.calls {
        blob.push(' ');
        blob.push_str(&call.arguments.to_string());
    }
    blob
}

fn describe(transcript: &Transcript) -> String {
    let Some(usage) = transcript.usage else {
        return "usage not reported".to_owned();
    };
    let rates = rates(usage, transcript.ttft, transcript.elapsed);
    format!(
        "{} in, {} out, {rates}",
        usage.input_tokens, usage.output_tokens
    )
}

fn rates(usage: TokenUsage, ttft: Option<Duration>, elapsed: Duration) -> String {
    let Some(ttft) = ttft else {
        return "timing too short to report".to_owned();
    };
    let decode = elapsed.saturating_sub(ttft);
    // A sub-millisecond clock reading is a scripted stream, not a measurement.
    // Dividing by it reports a nonsense rate.
    if ttft < Duration::from_millis(1)
        || decode < Duration::from_millis(1)
        || usage.input_tokens == 0
        || usage.output_tokens == 0
    {
        return "timing too short to report".to_owned();
    }
    let prefill = per_second(usage.input_tokens, ttft);
    let decode_rate = per_second(usage.output_tokens, decode);
    format!("prefill {prefill:.0} tok/s, decode {decode_rate:.0} tok/s")
}

#[allow(clippy::cast_precision_loss)]
fn per_second(tokens: u64, elapsed: Duration) -> f64 {
    tokens as f64 / elapsed.as_secs_f64()
}

fn prompt_evidence(blob: &str) -> (bool, String) {
    let missing: Vec<&str> = [
        (HEAD, "the head"),
        (TAIL, "the tail"),
        (SCHEMA, "the tool-description sentinel"),
    ]
    .into_iter()
    .filter_map(|(needle, label)| (!blob.contains(needle)).then_some(label))
    .collect();
    if missing.is_empty() {
        (
            true,
            "model repeated the head, the tail and the tool-description sentinel".to_owned(),
        )
    } else {
        (
            false,
            format!("model did not repeat {}", missing.join(", ")),
        )
    }
}

fn context_evidence(
    catalogue: Option<u32>,
    served: Option<u32>,
    tokens: u32,
    limits_error: Option<&str>,
    transcript: &Transcript,
    repeated: bool,
) -> String {
    let windows = format!(
        "catalogue {}, served {}, canary {tokens} tokens",
        window_label(catalogue),
        window_label(served),
    );
    let usage = match transcript.usage {
        Some(usage) => format!("server counted {} input tokens", usage.input_tokens),
        None => "usage not reported".to_owned(),
    };
    let outcome = if repeated {
        "model repeated the tail"
    } else {
        "model did not repeat the tail code"
    };
    match limits_error {
        Some(error) => format!("{error}; {windows}, {usage}, {outcome}"),
        None => format!("{windows}, {usage}, {outcome}"),
    }
}

fn window_label(window: Option<u32>) -> String {
    window.map_or_else(|| "unknown".to_owned(), |tokens| tokens.to_string())
}

/// An explicit length wins. Otherwise the canary sits `MARGIN` below the
/// served window, or the catalogue window when the server never said.
/// A window inside the margin cannot spare 4096 tokens, so the canary takes half.
fn canary_len(
    catalogue: Option<u32>,
    served: Option<u32>,
    override_tokens: Option<u32>,
) -> Result<u32, &'static str> {
    if let Some(tokens) = override_tokens.filter(|tokens| *tokens > 0) {
        return Ok(tokens);
    }
    match served.or(catalogue).filter(|tokens| *tokens > 0) {
        Some(basis) if basis > MARGIN => Ok(basis - MARGIN),
        Some(basis) => Ok(basis / 2),
        None => Err("profile declares no context window"),
    }
}

fn canary_message(tokens: u32) -> String {
    let mut message = String::from(
        "The last line of this message is a code. Repeat that code and nothing else.\n",
    );
    for _ in 0..tokens {
        message.push_str("pad ");
    }
    message.push('\n');
    message.push_str(WINDOW);
    message
}

fn prefill_message() -> String {
    let mut message = String::new();
    for _ in 0..PREFILL_TOKENS {
        message.push_str("pad ");
    }
    message.push_str("Reply with ok.");
    message
}

fn tool_system() -> String {
    format!("{TOOL_PHRASE}. Put {TOKEN} in its token argument.")
}

/// The head and tail are named by where they sit beside the instruction, not as
/// the prompt's first and last lines: Moonshot puts a preamble of its own first.
fn prompt_system() -> String {
    format!(
        "{HEAD}\nState the line just above this one, the line just below it, and the sentinel \
         that appears only in the tool description.\n{TAIL}"
    )
}

fn probe_tool() -> ToolDefinition {
    ToolDefinition {
        name: "probe_echo".into(),
        description: format!("Probe tool. Sentinel {SCHEMA}."),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "token": { "type": "string" }
            },
            "required": ["token"]
        }),
    }
}

fn plain_request(thinking: Thinking) -> SessionRequest {
    SessionRequest {
        system: PLAIN_SYSTEM.to_owned(),
        tools: Vec::new(),
        thinking,
        max_output_tokens: MAX_OUTPUT_TOKENS,
    }
}

fn passed(name: &'static str, evidence: impl Into<String>) -> Check {
    Check {
        name,
        passed: true,
        evidence: evidence.into(),
    }
}

fn failed(name: &'static str, evidence: impl Into<String>) -> Check {
    Check {
        name,
        passed: false,
        evidence: evidence.into(),
    }
}

fn print_human(report: &ProbeReport) {
    println!("{}", style::paint(PURPLE, &report.model));
    for check in &report.checks {
        let (colour, mark) = if check.passed {
            (OK, "pass")
        } else {
            (WARN, "fail")
        };
        println!(
            "  {}  {}  {}",
            style::paint(colour, mark),
            style::paint(PURPLE, &format!("{:<20}", check.name)),
            style::paint(FAINT, &check.evidence)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use anyhow::bail;
    use dino_model::ModelEventStream;
    use dino_model::ModelFuture;
    use dino_protocol::ReasoningEffort;
    use dino_protocol::StopReason;
    use futures_util::stream;
    use serde_json::json;

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Fault {
        Honest,
        ToolAsText,
        DropTailSentinel,
        ThinksAnyway,
        NeverThinks,
        DropContextTail,
        Transport,
        /// Reasons for this many tokens before any answer, all counted against
        /// the cap, as Kimi does on Moonshot.
        ReasonsFirst(u32),
        /// Turns down every request that carries tool results.
        RefusesHistory,
        IgnoresNote,
        /// Breaks partway through any answer to tool results.
        BreaksHistory,
    }

    #[derive(Clone, Debug, Default)]
    struct Record {
        thinking: Vec<Thinking>,
        systems: Vec<String>,
        tool_descriptions: Vec<String>,
        users: Vec<String>,
        /// Each history a session was given, in its own provider's shape.
        restored: Vec<serde_json::Value>,
        /// The call ids of each set of tool results, in the order sent.
        results: Vec<Vec<String>>,
    }

    struct ScriptedHost {
        profile: ModelProfile,
        fault: Fault,
        served: Option<u32>,
        limits_error: bool,
        refuse_open: bool,
        double_usage: bool,
        record: Arc<Mutex<Record>>,
    }

    struct ScriptedSession {
        profile: ModelProfile,
        fault: Fault,
        thinking: Thinking,
        system: String,
        max_output_tokens: u32,
        served: Option<u32>,
        limits_error: bool,
        double_usage: bool,
        record: Arc<Mutex<Record>>,
    }

    impl ScriptedHost {
        fn new(key: &str, fault: Fault) -> Self {
            Self {
                profile: dino_model::builtin_profile(key).expect("built-in profile"),
                fault,
                served: None,
                limits_error: false,
                refuse_open: false,
                double_usage: false,
                record: Arc::new(Mutex::new(Record::default())),
            }
        }

        fn snapshot(&self) -> Record {
            self.record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl ProbeHost for ScriptedHost {
        fn open(&mut self, request: SessionRequest) -> Result<Box<dyn ModelSession>> {
            if self.refuse_open {
                bail!(
                    "model {} needs QWEN_API_BASE, which is not set in the environment",
                    self.profile.key
                );
            }
            {
                let mut record = self
                    .record
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                record.thinking.push(request.thinking);
                record.systems.push(request.system.clone());
                record
                    .tool_descriptions
                    .extend(request.tools.iter().map(|tool| tool.description.clone()));
            }
            Ok(Box::new(ScriptedSession {
                profile: self.profile.clone(),
                fault: self.fault,
                thinking: request.thinking,
                system: request.system,
                max_output_tokens: request.max_output_tokens,
                served: self.served,
                limits_error: self.limits_error,
                double_usage: self.double_usage,
                record: Arc::clone(&self.record),
            }))
        }
    }

    impl ModelSession for ScriptedSession {
        fn profile(&self) -> &ModelProfile {
            &self.profile
        }

        fn discover_limits(&mut self) -> ModelFuture<'_, ()> {
            if self.limits_error {
                return Box::pin(async {
                    Err(ModelError::Transport("models endpoint refused".into()))
                });
            }
            if let Some(served) = self.served {
                let window = &mut self.profile.context_window_tokens;
                *window = Some(window.map_or(served, |native| native.min(served)));
            }
            Box::pin(async { Ok(()) })
        }

        fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
            let user = match &input {
                TurnInput::User { content } | TurnInput::UserWithImages { content, .. } => {
                    content.clone()
                }
                TurnInput::ToolResults { note, .. } => note.clone().unwrap_or_default(),
            };
            {
                let mut record = self
                    .record
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                record.users.push(user.clone());
                if let TurnInput::ToolResults { results, .. } = &input {
                    record.results.push(
                        results
                            .iter()
                            .map(|result| result.call_id.clone())
                            .collect(),
                    );
                }
            }
            if self.fault == Fault::Transport {
                return Box::pin(async { Err(ModelError::Transport("connection reset".into())) });
            }
            let events = match &input {
                TurnInput::ToolResults { .. } if self.fault == Fault::RefusesHistory => {
                    return Box::pin(async {
                        Err(ModelError::Http {
                            provider: "Qwen endpoint",
                            status: 400,
                            body: "invalid messages".into(),
                        })
                    });
                }
                TurnInput::ToolResults { .. } => self.history_events(&user),
                _ => self.events(&user),
            };
            Box::pin(async move { Ok(Box::pin(stream::iter(events)) as ModelEventStream) })
        }

        fn export_state(&self) -> Result<SessionState, ModelError> {
            Ok(SessionState {
                provider: self.profile.provider,
                model_key: self.profile.key.clone(),
                version: 1,
                payload: json!([]),
            })
        }

        fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
            self.record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .restored
                .push(state.payload.clone());
            Ok(())
        }
    }

    impl ScriptedSession {
        fn history_events(&self, note: &str) -> Vec<std::result::Result<ModelEvent, ModelError>> {
            match self.fault {
                Fault::BreaksHistory => vec![
                    Ok(ModelEvent::TextDelta { text: "o".into() }),
                    Err(ModelError::Transport("connection reset".into())),
                ],
                Fault::IgnoresNote => text_events("ok", false, false),
                _ if note.contains(NOTE) => text_events(NOTE, false, false),
                _ => text_events("ok", false, false),
            }
        }

        fn events(&self, user: &str) -> Vec<std::result::Result<ModelEvent, ModelError>> {
            if user.contains(WINDOW) {
                let text = if self.fault == Fault::DropContextTail {
                    "gone"
                } else {
                    WINDOW
                };
                return text_events(text, false, self.double_usage);
            }
            let answer_follows_reasoning =
                self.system.contains(TOOL_PHRASE) || self.system.contains(HEAD);
            if let Fault::ReasonsFirst(reasoning) = self.fault
                && answer_follows_reasoning
                && self.max_output_tokens <= reasoning
            {
                return text_events("", true, self.double_usage);
            }
            if self.system.contains(TOOL_PHRASE) {
                if self.fault == Fault::ToolAsText {
                    return text_events(&format!("{TOKEN} in prose"), false, self.double_usage);
                }
                return tool_events();
            }
            if self.system.contains(HEAD) {
                let mut text = format!("{HEAD}\n{SCHEMA}");
                if self.fault != Fault::DropTailSentinel {
                    text.push('\n');
                    text.push_str(TAIL);
                }
                return text_events(&text, false, self.double_usage);
            }
            let think = match self.fault {
                Fault::NeverThinks => false,
                Fault::ThinksAnyway => true,
                Fault::Honest
                | Fault::ToolAsText
                | Fault::DropTailSentinel
                | Fault::DropContextTail
                | Fault::Transport
                | Fault::RefusesHistory
                | Fault::IgnoresNote
                | Fault::BreaksHistory
                | Fault::ReasonsFirst(_) => self.thinking == Thinking::On,
            };
            text_events("ok", think, self.double_usage)
        }
    }

    fn text_events(
        text: &str,
        think: bool,
        double_usage: bool,
    ) -> Vec<std::result::Result<ModelEvent, ModelError>> {
        let mut events = Vec::new();
        let reasoning_tokens = if think { 3 } else { 0 };
        if think {
            events.push(Ok(ModelEvent::ReasoningDelta {
                text: "because".into(),
            }));
        }
        events.push(Ok(ModelEvent::TextDelta {
            text: text.to_owned(),
        }));
        if double_usage {
            events.push(Ok(usage_event(1, reasoning_tokens)));
        }
        let input_tokens = if double_usage { 64 } else { 16 };
        events.push(Ok(usage_event(input_tokens, reasoning_tokens)));
        events.push(Ok(ModelEvent::Finished {
            reason: StopReason::EndTurn,
        }));
        events
    }

    fn tool_events() -> Vec<std::result::Result<ModelEvent, ModelError>> {
        vec![
            Ok(ModelEvent::ToolCallCompleted {
                call: ToolCall {
                    id: "probe-1".into(),
                    name: "probe_echo".into(),
                    arguments: json!({ "token": TOKEN }),
                },
            }),
            Ok(usage_event(16, 0)),
            Ok(ModelEvent::Finished {
                reason: StopReason::ToolUse,
            }),
        ]
    }

    fn usage_event(input_tokens: u64, reasoning_tokens: u64) -> ModelEvent {
        ModelEvent::Usage {
            usage: TokenUsage {
                input_tokens,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 1,
                reasoning_tokens,
            },
        }
    }

    fn options(profile: &ModelProfile, canary_tokens: Option<u32>) -> ProbeOptions {
        options_from(profile, canary_tokens, false)
    }

    async fn probe(host: &mut ScriptedHost, options: &ProbeOptions) -> ProbeReport {
        run_probe(host, options).await.expect("probe")
    }

    fn named<'a>(report: &'a ProbeReport, name: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|check| check.name == name)
            .expect("named check")
    }

    async fn only(fault: Fault, name: &str) -> Check {
        let mut host = ScriptedHost::new("qwen3.7-max", fault);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        assert!(!report.passed, "{report:?}");
        let check = named(&report, name).clone();
        assert!(!check.passed, "{check:?}");
        for other in &report.checks {
            if other.name != name {
                assert!(other.passed, "{} should pass: {other:?}", other.name);
            }
        }
        check
    }

    #[test]
    fn the_canary_sits_below_the_served_window() {
        assert_eq!(canary_len(Some(262_144), Some(8192), None), Ok(4096));
        assert_eq!(canary_len(Some(8192), None, None), Ok(4096));
        assert_eq!(canary_len(Some(262_144), None, Some(32)), Ok(32));
        // Zero is not a length. It falls through to the window.
        assert_eq!(canary_len(Some(8192), None, Some(0)), Ok(4096));
        assert_eq!(canary_len(Some(4096), None, None), Ok(2048));
        assert_eq!(
            canary_len(None, None, None),
            Err("profile declares no context window")
        );
    }

    #[test]
    fn sentinels_sit_only_where_a_copy_of_the_middle_cannot_reach() {
        let system = prompt_system();
        assert!(system.starts_with(HEAD));
        assert!(system.ends_with(TAIL));
        let middle = &system[HEAD.len()..system.len() - TAIL.len()];
        assert!(!middle.contains(HEAD));
        assert!(!middle.contains(TAIL));
        assert!(!middle.contains(SCHEMA));
        assert!(!middle.contains(TOKEN));
        let tool = tool_system();
        assert!(tool.contains(TOOL_PHRASE));
        assert!(tool.contains(TOKEN));
        assert!(!tool.contains(HEAD));
        assert!(probe_tool().description.contains(SCHEMA));
        assert!(!PROMPT_USER.contains(HEAD));
        assert!(!PROMPT_USER.contains(TAIL));
        assert!(!PROMPT_USER.contains(SCHEMA));
    }

    #[test]
    fn throughput_reports_rates_and_refuses_a_zero_clock() {
        let usage = TokenUsage {
            input_tokens: 256,
            output_tokens: 8,
            ..TokenUsage::default()
        };
        let measured = Transcript {
            usage: Some(usage),
            ttft: Some(Duration::from_secs(1)),
            elapsed: Duration::from_secs(5),
            ..Transcript::default()
        };
        assert_eq!(
            describe(&measured),
            "256 in, 8 out, prefill 256 tok/s, decode 2 tok/s"
        );
        let fast = Transcript {
            usage: Some(usage),
            ttft: Some(Duration::from_micros(10)),
            elapsed: Duration::from_micros(20),
            ..Transcript::default()
        };
        assert_eq!(describe(&fast), "256 in, 8 out, timing too short to report");
        let silent = TokenUsage {
            input_tokens: 256,
            ..TokenUsage::default()
        };
        let no_output = Transcript {
            usage: Some(silent),
            ttft: Some(Duration::from_secs(1)),
            elapsed: Duration::from_secs(5),
            ..Transcript::default()
        };
        assert_eq!(
            describe(&no_output),
            "256 in, 0 out, timing too short to report"
        );
        assert_eq!(describe(&Transcript::default()), "usage not reported");
    }

    #[test]
    fn reasoning_counts_from_either_channel() {
        let tokens_only = Transcript {
            usage: Some(TokenUsage {
                reasoning_tokens: 4,
                ..TokenUsage::default()
            }),
            ..Transcript::default()
        };
        assert!(saw_reasoning(&tokens_only));
        let text_only = Transcript {
            reasoning: "because".into(),
            ..Transcript::default()
        };
        assert!(saw_reasoning(&text_only));
        assert!(!saw_reasoning(&Transcript::default()));
    }

    #[tokio::test]
    async fn an_honest_endpoint_passes_every_check() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        assert!(report.passed, "{report:?}");
        assert_eq!(
            report
                .checks
                .iter()
                .map(|check| check.name)
                .collect::<Vec<_>>(),
            vec![
                TOOL_PARSER,
                PROMPT_AND_SCHEMA,
                THINKING,
                THROUGHPUT,
                NOTE_AFTER_RESULTS,
                ORPHANED_RESULT,
                DUPLICATE_CALL_ID,
                RESULTS_OUT_OF_ORDER,
                CONTEXT_WINDOW
            ]
        );
        let value = serde_json::to_value(&report).expect("json");
        assert_eq!(value["model"], "qwen3.7-max");
        assert_eq!(value["passed"], true);
        assert_eq!(value["checks"][0]["name"], "tool_parser");
        assert!(
            named(&report, THINKING)
                .evidence
                .contains("thinking on produced reasoning")
        );
        let throughput = &named(&report, THROUGHPUT).evidence;
        assert!(throughput.contains("trial 1:"), "{throughput}");
        assert!(throughput.contains("trial 3:"), "{throughput}");
        let record = host.snapshot();
        assert!(record.thinking.contains(&Thinking::On));
        assert!(record.thinking.contains(&Thinking::Off));
        let canary = record
            .users
            .iter()
            .find(|user| user.contains(WINDOW))
            .expect("canary");
        assert!(canary.ends_with(WINDOW), "{canary}");
        assert_eq!(canary.matches("pad ").count(), 32);
        assert_eq!(
            record
                .users
                .iter()
                .filter(|user| user.matches("pad ").count()
                    == usize::try_from(PREFILL_TOKENS).unwrap_or(0))
                .count(),
            TRIALS
        );
        assert!(
            record
                .systems
                .iter()
                .any(|system| system.starts_with(HEAD) && system.ends_with(TAIL))
        );
        assert!(
            record
                .tool_descriptions
                .iter()
                .any(|description| description.contains(SCHEMA))
        );
        assert!(
            named(&report, CONTEXT_WINDOW)
                .evidence
                .contains("model repeated the tail")
        );
    }

    #[tokio::test]
    async fn a_model_that_reasons_before_answering_still_passes() {
        let mut host = ScriptedHost::new("kimi-k3.1-pro", Fault::ReasonsFirst(521));
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        assert!(report.passed, "{report:?}");
    }

    #[tokio::test]
    async fn skipping_the_context_window_sends_no_canary() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
        let mut opts = options(&host.profile, None);
        opts.context_window = false;
        let report = probe(&mut host, &opts).await;
        assert!(report.passed, "{report:?}");
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.name != CONTEXT_WINDOW)
        );
        assert!(
            !host
                .snapshot()
                .users
                .iter()
                .any(|user| user.contains(WINDOW))
        );
    }

    #[tokio::test]
    async fn text_standing_in_for_a_tool_call_fails_that_check() {
        let check = only(Fault::ToolAsText, TOOL_PARSER).await;
        assert!(
            check.evidence.contains("arrived as text"),
            "{}",
            check.evidence
        );
        assert!(check.evidence.contains(TOKEN), "{}", check.evidence);
    }

    #[tokio::test]
    async fn a_dropped_tail_fails_the_prompt_check() {
        let check = only(Fault::DropTailSentinel, PROMPT_AND_SCHEMA).await;
        assert!(
            check.evidence.contains("model did not repeat the tail"),
            "{}",
            check.evidence
        );
        assert!(!check.evidence.contains("head"), "{}", check.evidence);
    }

    #[tokio::test]
    async fn thinking_that_stays_on_fails() {
        let check = only(Fault::ThinksAnyway, THINKING).await;
        assert!(check.evidence.contains("stayed on"), "{}", check.evidence);
    }

    #[tokio::test]
    async fn thinking_that_never_starts_fails() {
        let check = only(Fault::NeverThinks, THINKING).await;
        assert!(
            check.evidence.contains("no reasoning"),
            "{}",
            check.evidence
        );
    }

    #[tokio::test]
    async fn a_profile_with_no_thinking_control_passes() {
        let mut host = ScriptedHost::new("qwen3.8-27b", Fault::Honest);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        assert!(report.passed, "{report:?}");
        assert!(
            named(&report, THINKING)
                .evidence
                .contains("no thinking control"),
            "{}",
            named(&report, THINKING).evidence
        );
        let modes = host.snapshot().thinking;
        assert!(!modes.contains(&Thinking::On));
        assert!(!modes.contains(&Thinking::Off));
    }

    #[tokio::test]
    async fn effort_and_provider_managed_thinking_are_not_flipped() {
        for reasoning in [
            ReasoningSupport::Effort(vec![ReasoningEffort::Low]),
            ReasoningSupport::ProviderManaged,
        ] {
            let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
            let mut opts = options(&host.profile, Some(32));
            opts.reasoning = reasoning;
            let report = probe(&mut host, &opts).await;
            assert!(report.passed, "{report:?}");
            assert!(
                named(&report, THINKING)
                    .evidence
                    .contains("not a boolean this probe flips"),
                "{}",
                named(&report, THINKING).evidence
            );
            let modes = host.snapshot().thinking;
            assert!(!modes.contains(&Thinking::On));
            assert!(!modes.contains(&Thinking::Off));
        }
    }

    #[tokio::test]
    async fn a_dropped_context_tail_fails() {
        let check = only(Fault::DropContextTail, CONTEXT_WINDOW).await;
        assert!(
            check
                .evidence
                .contains("model did not repeat the tail code"),
            "{}",
            check.evidence
        );
    }

    #[tokio::test]
    async fn a_transport_error_fails_the_check_and_does_not_abort() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Transport);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        assert!(!report.passed);
        assert_eq!(report.checks.len(), 9);
        for check in &report.checks {
            assert!(!check.passed, "{check:?}");
            assert!(
                check.evidence.contains("connection reset"),
                "{}",
                check.evidence
            );
        }
    }

    #[tokio::test]
    async fn a_session_that_will_not_open_aborts() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
        host.refuse_open = true;
        let opts = options(&host.profile, Some(32));
        let error = run_probe(&mut host, &opts).await.expect_err("open");
        assert!(error.to_string().contains("QWEN_API_BASE"), "{error}");
    }

    #[tokio::test]
    async fn a_limits_failure_is_evidence_and_the_canary_still_runs() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
        host.limits_error = true;
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        assert!(report.passed, "{report:?}");
        let evidence = &named(&report, CONTEXT_WINDOW).evidence;
        assert!(evidence.contains("models endpoint refused"), "{evidence}");
        assert!(evidence.contains("served unknown"), "{evidence}");
        assert!(evidence.contains("model repeated the tail"), "{evidence}");
        let canary = host
            .snapshot()
            .users
            .into_iter()
            .find(|user| user.contains(WINDOW))
            .expect("canary");
        assert_eq!(canary.matches("pad ").count(), 32);
        assert!(canary.ends_with(WINDOW));
    }

    #[tokio::test]
    async fn the_served_window_shrinks_the_canary() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
        host.served = Some(8192);
        let opts = options(&host.profile, None);
        let report = probe(&mut host, &opts).await;
        assert!(report.passed, "{report:?}");
        let evidence = &named(&report, CONTEXT_WINDOW).evidence;
        assert!(evidence.contains("catalogue 262144"), "{evidence}");
        assert!(evidence.contains("served 8192"), "{evidence}");
        assert!(evidence.contains("canary 4096 tokens"), "{evidence}");
        assert!(evidence.contains("server counted"), "{evidence}");
        let canary = host
            .snapshot()
            .users
            .into_iter()
            .find(|user| user.ends_with(WINDOW))
            .expect("canary");
        assert_eq!(canary.matches("pad ").count(), 4096);
    }

    #[tokio::test]
    async fn the_last_usage_event_is_the_one_reported() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
        host.double_usage = true;
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        let evidence = &named(&report, THROUGHPUT).evidence;
        assert!(named(&report, THROUGHPUT).passed, "{evidence}");
        assert!(evidence.contains("64 in"), "{evidence}");
        assert!(!evidence.contains("65 in"), "{evidence}");
    }

    const HISTORIES: [&str; 4] = [
        NOTE_AFTER_RESULTS,
        ORPHANED_RESULT,
        DUPLICATE_CALL_ID,
        RESULTS_OUT_OF_ORDER,
    ];

    #[tokio::test]
    async fn each_history_is_handed_over_and_answered() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        assert!(report.passed, "{report:?}");
        for name in HISTORIES {
            let evidence = &named(&report, name).evidence;
            assert!(evidence.starts_with("answered: "), "{name}: {evidence}");
        }
        assert!(named(&report, NOTE_AFTER_RESULTS).evidence.contains(NOTE));
        let record = host.snapshot();
        assert_eq!(record.restored.len(), 4);
        let ids = |history: &serde_json::Value| -> Vec<String> {
            history
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|message| {
                    message["tool_calls"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|call| call["id"].as_str().map(str::to_owned))
                .collect()
        };
        assert_eq!(ids(&record.restored[2]), ["probe-1", "probe-1"]);
        assert_eq!(
            record.results,
            [
                vec!["probe-1"],
                vec!["probe-ghost"],
                vec!["probe-1"],
                vec!["probe-b", "probe-a"],
            ]
        );
    }

    #[tokio::test]
    async fn an_anthropic_session_gets_the_histories_in_its_own_shape() {
        let mut host = ScriptedHost::new("claude-haiku-5.5", Fault::Honest);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        for name in HISTORIES {
            assert!(named(&report, name).passed, "{report:?}");
        }
        let restored = host.snapshot().restored;
        let duplicated = restored[2].to_string();
        assert_eq!(
            duplicated.matches("\"tool_use\"").count(),
            2,
            "{duplicated}"
        );
        assert!(!duplicated.contains("tool_calls"), "{duplicated}");
    }

    #[tokio::test]
    async fn an_openai_session_keeps_its_history_on_the_server_and_is_not_seeded() {
        let mut host = ScriptedHost::new("terra", Fault::Honest);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        for name in HISTORIES {
            let check = named(&report, name);
            assert!(check.passed, "{check:?}");
            assert!(check.evidence.contains("not applicable"), "{check:?}");
        }
        assert!(host.snapshot().restored.is_empty());
    }

    #[tokio::test]
    async fn a_refusal_will_do_for_every_history_but_the_note() {
        let check = only(Fault::RefusesHistory, NOTE_AFTER_RESULTS).await;
        assert!(check.evidence.contains("400 Bad Request"), "{check:?}");
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::RefusesHistory);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        for name in &HISTORIES[1..] {
            let evidence = &named(&report, name).evidence;
            assert!(evidence.starts_with("refused: "), "{name}: {evidence}");
        }
    }

    #[tokio::test]
    async fn a_note_the_answer_ignores_fails() {
        let check = only(Fault::IgnoresNote, NOTE_AFTER_RESULTS).await;
        assert!(check.evidence.contains("answered without"), "{check:?}");
    }

    #[tokio::test]
    async fn a_history_answer_that_breaks_fails_every_history_and_nothing_else() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::BreaksHistory);
        let opts = options(&host.profile, Some(32));
        let report = probe(&mut host, &opts).await;
        for check in &report.checks {
            assert_eq!(check.passed, !HISTORIES.contains(&check.name), "{check:?}");
        }
    }

    #[tokio::test]
    async fn skipping_the_histories_sends_none() {
        let mut host = ScriptedHost::new("qwen3.7-max", Fault::Honest);
        let mut opts = options(&host.profile, Some(32));
        opts.history = false;
        let report = probe(&mut host, &opts).await;
        assert!(report.passed, "{report:?}");
        assert!(
            report
                .checks
                .iter()
                .all(|check| !HISTORIES.contains(&check.name))
        );
        let record = host.snapshot();
        assert!(record.restored.is_empty());
        assert!(record.results.is_empty());
    }
}
