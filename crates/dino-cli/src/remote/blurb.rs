//! The line above the cards in `dino dash --blurb`: a model's dry summary of
//! how the fleet is getting on.
//!
//! The model is shown a [`Fact`] per session and nothing else. A `Fact` has no
//! text field: its words are the fixed ones in `dash.rs` (a state, a kind of
//! activity, a kind of trouble) and numbers. No task, path, command, tool
//! output, note, model reply or workspace name can reach it, because there is
//! nowhere in the type for one to go.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use super::dash::Card;
use super::dash::State;

const MODEL: &str = "claude-haiku-5-5";
const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// Between questions, whatever has changed. Haiku is cheap; it is not free.
const SPACING: Duration = Duration::from_secs(60);

const SYSTEM: &str = "You report to an engineer on a small fleet of coding-agent sessions \
running on his machine. You are given only states, counts and durations, never what any \
session is working on, and you must not guess at that. Write two sentences at most. Say first \
whether anything needs attention: stuck, dead, failed, or waiting for an approval. Then, if \
you must, one dry remark. The tone is British understatement, a country diary rather than a \
chat window: no exclamation marks, no emoji, no em dashes. The facts are exact; the humour is \
tone and never invention. Sessions are named by letter.";

#[derive(Debug, Serialize)]
pub struct Fact {
    session: String,
    state: State,
    doing: &'static str,
    trouble: Vec<&'static str>,
    quiet_seconds: u64,
    turn: u32,
    max_turns: u32,
    calls: usize,
}

impl Fact {
    /// The facts of the sessions worth a remark: live ones, and ones that failed
    /// or died. Finished ones have nothing to report.
    #[must_use]
    pub fn of(cards: &[Card], now: u64) -> Vec<Self> {
        cards
            .iter()
            .filter(|card| card.state != State::Done)
            .enumerate()
            .map(|(index, card)| Self {
                session: letter(index),
                state: card.state,
                doing: card.kind,
                trouble: card.trouble.clone(),
                quiet_seconds: now.saturating_sub(card.last_ms) / 1_000,
                turn: card.turn,
                max_turns: card.max_turns,
                calls: card.calls,
            })
            .collect()
    }
}

fn letter(index: usize) -> String {
    match u8::try_from(index) {
        Ok(index) if index < 26 => char::from(b'A' + index).to_string(),
        _ => format!("S{index}"),
    }
}

/// What the fleet looks like without its clocks: a change here is worth asking
/// again about, a second ticking by is not.
fn shape(facts: &[Fact]) -> String {
    facts
        .iter()
        .map(|fact| {
            format!(
                "{}:{:?}:{}:{}",
                fact.session,
                fact.state,
                fact.doing,
                fact.trouble.join("+")
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Default)]
struct Cache {
    shape: String,
    text: Option<String>,
    asked: Option<Instant>,
    in_flight: bool,
}

pub struct Blurb {
    client: reqwest::Client,
    key: String,
    cache: Mutex<Cache>,
}

impl Blurb {
    /// # Errors
    ///
    /// Returns an error when `ANTHROPIC_API_KEY` is not set.
    pub fn from_env() -> Result<Arc<Self>> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .context("--blurb needs ANTHROPIC_API_KEY in the environment")?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("cannot build the HTTP client")?;
        Ok(Arc::new(Self {
            client,
            key,
            cache: Mutex::new(Cache::default()),
        }))
    }

    /// The latest summary, asking for a new one in the background when the
    /// fleet has changed shape and the last question is old enough.
    pub fn text(self: &Arc<Self>, facts: Vec<Fact>) -> Option<String> {
        let shape = shape(&facts);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if facts.is_empty() {
            return None;
        }
        let due = cache.asked.is_none_or(|asked| asked.elapsed() >= SPACING);
        if cache.shape != shape && !cache.in_flight && due {
            cache.in_flight = true;
            cache.asked = Some(Instant::now());
            let this = Arc::clone(self);
            tokio::spawn(async move {
                let answer = this.ask(&facts).await;
                let mut cache = this
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.in_flight = false;
                match answer {
                    Ok(text) => {
                        cache.text = Some(text);
                        cache.shape = shape;
                    }
                    Err(error) => eprintln!("dino: dash: no blurb: {error:#}"),
                }
            });
        }
        cache.text.clone()
    }

    async fn ask(&self, facts: &[Fact]) -> Result<String> {
        let body = serde_json::json!({
            "model": MODEL,
            "max_tokens": 160,
            "system": SYSTEM,
            "messages": [{ "role": "user", "content": serde_json::to_string(facts)? }],
        });
        let response = self
            .client
            .post(ENDPOINT)
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .context("the request failed")?;
        let status = response.status();
        let reply: Value = response.json().await.context("unreadable reply")?;
        if !status.is_success() {
            let message = reply
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            anyhow::bail!("{status}: {message}");
        }
        reply
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .context("an empty reply")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::dash::Digest;

    fn card(task: &str, path: &str, now_offset: u64) -> Card {
        let mut digest = Digest::new(std::path::Path::new("secret-session.jsonl"));
        let lines = [
            r#"{"record":"started","seq":0,"unix_millis":1000,"session":{"session_id":"s","dino_version":"0","model_key":"secret-model","provider":"qwen","workspace_root":"/home/secret-project","approval_mode":"ask","sandbox":"none","max_model_turns":100,"pid":7}}"#.to_owned(),
            format!(
                r#"{{"record":"event","seq":1,"unix_millis":1001,"event":{{"type":"submitted","text":"{task}"}}}}"#
            ),
            format!(
                r#"{{"record":"event","seq":2,"unix_millis":1002,"event":{{"type":"model","model_turn":1,"event":{{"type":"tool_call_completed","call":{{"id":"c","name":"read","arguments":{{"path":"{path}"}}}}}}}}}}"#
            ),
            r#"{"record":"event","seq":3,"unix_millis":1003,"event":{"type":"noted","model_turn":1,"call_id":"c","note":"secret-note"}}"#.to_owned(),
        ];
        for line in &lines {
            digest.line(line);
        }
        digest.card(1_003 + now_offset, Some(true))
    }

    #[test]
    fn what_the_model_is_sent_holds_nothing_of_the_session() {
        let cards = [card(
            "rotate the SECRET-TASK keys",
            "src/secret/path.rs",
            90_000,
        )];
        let sent = serde_json::to_string(&Fact::of(&cards, 1_003 + 90_000)).unwrap();
        for word in ["SECRET", "secret", "path.rs", "rotate", "dino-cli"] {
            assert!(!sent.contains(word), "{word} reached the facts: {sent}");
        }
        assert!(sent.contains("needs_you"), "{sent}");
    }

    #[test]
    fn the_clock_alone_does_not_change_the_shape() {
        let early = Fact::of(&[card("t", "a.rs", 10_000)], 11_003);
        let late = Fact::of(&[card("t", "a.rs", 20_000)], 21_003);
        assert_eq!(shape(&early), shape(&late));
    }
}
