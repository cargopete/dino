//! Noticing a model that is going round in circles.
//!
//! Calls are compared by tool name and arguments, in the order they ran within
//! one run. A repeat is nudged once it is clearly a habit, and refused before it
//! becomes the whole run.

use dino_protocol::ToolCall;

/// Identical calls in a row before the model is told.
const NUDGE_AT: usize = 3;
/// Identical calls in a row that are refused rather than run.
const REFUSE_AT: usize = 5;
/// Calls over which two alternating calls count as a loop.
const ALTERNATION: usize = 6;

#[derive(Debug, Default)]
pub struct LoopWatch {
    calls: Vec<String>,
}

impl LoopWatch {
    pub fn reset(&mut self) {
        self.calls.clear();
    }

    /// A refusal for a call that would be the [`REFUSE_AT`]th identical one in
    /// a row, or `None` to run it.
    #[must_use]
    pub fn refuse(&self, call: &ToolCall) -> Option<String> {
        let signature = signature(call);
        (self.repeats(&signature) + 1 >= REFUSE_AT).then(|| {
            format!(
                "refused: `{}` was sent {} times in a row with these exact arguments, so it \
                 was not run again. Its result has not changed. Use what you already have, \
                 change the approach, or say what is stopping you.",
                call.name,
                REFUSE_AT - 1
            )
        })
    }

    /// Records a call that ran, and returns a note for the model when the
    /// recent calls look like a loop.
    pub fn record(&mut self, call: &ToolCall) -> Option<String> {
        let signature = signature(call);
        let repeats = self.repeats(&signature) + 1;
        self.calls.push(signature);
        if repeats == NUDGE_AT {
            return Some(format!(
                "[dino: `{}` has now been called {NUDGE_AT} times in a row with identical \
                 arguments, so its result will not change. Use the result you have, or change \
                 the approach.]",
                call.name
            ));
        }
        self.alternating().then(|| {
            "[dino: the last calls alternate between the same two, with the same arguments, \
             and nothing has changed. Step back and try a different approach.]"
                .to_owned()
        })
    }

    /// How many of the latest calls in a row match `signature`.
    fn repeats(&self, signature: &str) -> usize {
        self.calls
            .iter()
            .rev()
            .take_while(|previous| *previous == signature)
            .count()
    }

    /// Whether the latest calls run A, B, A, B, … with A and B different. Said
    /// once, when the pattern first reaches its length.
    fn alternating(&self) -> bool {
        let n = self.calls.len();
        if n < ALTERNATION {
            return false;
        }
        let window = &self.calls[n - ALTERNATION..];
        let (a, b) = (&window[0], &window[1]);
        let alternates = a != b
            && window
                .iter()
                .enumerate()
                .all(|(index, call)| call == if index % 2 == 0 { a } else { b });
        let longer = n > ALTERNATION && self.calls[n - ALTERNATION - 1] == *b;
        alternates && !longer
    }
}

fn signature(call: &ToolCall) -> String {
    format!("{}\u{1f}{}", call.name, call.arguments)
}

#[cfg(test)]
mod tests {
    use super::LoopWatch;
    use dino_protocol::ToolCall;
    use serde_json::json;

    fn call(name: &str, path: &str) -> ToolCall {
        ToolCall {
            id: "id".into(),
            name: name.into(),
            arguments: json!({"path": path}),
        }
    }

    #[test]
    fn the_third_identical_call_is_nudged_and_the_fifth_refused() {
        let mut watch = LoopWatch::default();
        let same = call("exec", "a");
        assert_eq!(watch.record(&same), None);
        assert_eq!(watch.record(&same), None);
        assert!(watch.record(&same).unwrap().contains("3 times in a row"));
        assert_eq!(watch.refuse(&same), None);
        assert_eq!(watch.record(&same), None);
        assert!(watch.refuse(&same).unwrap().starts_with("refused"));
        assert_eq!(watch.refuse(&call("exec", "b")), None);
    }

    #[test]
    fn a_different_call_in_between_resets_the_count() {
        let mut watch = LoopWatch::default();
        let same = call("exec", "a");
        watch.record(&same);
        watch.record(&same);
        watch.record(&call("apply_patch", "src/lib.rs"));
        assert_eq!(watch.record(&same), None);
    }

    #[test]
    fn alternation_is_noticed_once() {
        let mut watch = LoopWatch::default();
        let (a, b) = (call("read", "a"), call("read", "b"));
        let notes: Vec<bool> = [&a, &b, &a, &b, &a, &b, &a]
            .into_iter()
            .map(|call| watch.record(call).is_some())
            .collect();
        assert_eq!(notes, [false, false, false, false, false, true, false]);
    }
}
