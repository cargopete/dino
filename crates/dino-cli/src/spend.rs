//! What a session has cost at list price, and what today has.
//!
//! Priced as each response arrives rather than by the renderer, because a
//! fallback can change the model partway through a submission and only the
//! session knows which one answered.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use dino_model::Elision;
use dino_model::ModelError;
use dino_model::ModelEventStream;
use dino_model::ModelFuture;
use dino_model::ModelSession;
use dino_model::SessionState;
use dino_protocol::ModelEvent;
use dino_protocol::ModelProfile;
use dino_protocol::Pricing;
use dino_protocol::TokenUsage;
use dino_protocol::TurnInput;
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde::Serialize;

/// This session's spending, and where today's is kept.
#[derive(Debug, Default)]
pub struct Meter {
    spent: Mutex<Spent>,
    ledger: Option<Ledger>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spent {
    pub usd: f64,
    /// Responses from a model without a list price, which `usd` leaves out.
    pub unpriced: u64,
}

impl Meter {
    #[must_use]
    pub fn new(ledger: Option<Ledger>) -> Self {
        Self {
            spent: Mutex::new(Spent::default()),
            ledger,
        }
    }

    pub fn record(&self, model: &str, price: Option<Pricing>, usage: &TokenUsage) {
        let usd = price.map(|price| price.cost(usage));
        if let Ok(mut spent) = self.spent.lock() {
            match usd {
                Some(usd) => spent.usd += usd,
                None => spent.unpriced += 1,
            }
        }
        if let Some(ledger) = &self.ledger {
            ledger.append(model, usd, usage);
        }
    }

    /// Kept in Dino's state directory, where there is one.
    #[must_use]
    pub fn for_session(session: &str) -> Self {
        Self::new(crate::settings::state_dir().map(|dir| Ledger::new(&dir, session)))
    }

    #[must_use]
    pub fn spent(&self) -> Spent {
        self.spent.lock().map(|spent| *spent).unwrap_or_default()
    }

    /// Today's total across every session that kept a ledger, this one included.
    #[must_use]
    pub fn today(&self) -> Option<f64> {
        self.ledger.as_ref().map(Ledger::today)
    }

    /// What a session leaving says it cost.
    #[must_use]
    pub fn closing(&self) -> String {
        let spent = self.spent();
        let mut line = format!("{} this session", usd(spent.usd));
        if let Some(today) = self.today() {
            let _ = write!(line, " · {} today (UTC)", usd(today));
        }
        line.push_str(", at list price");
        if spent.unpriced > 0 {
            let _ = write!(
                line,
                "; {} response(s) from a model without a list price are not counted",
                spent.unpriced
            );
        }
        line
    }
}

/// One line per priced response, in a file per UTC day under Dino's state
/// directory. Appended rather than rewritten, so sessions running at once do
/// not overwrite each other's totals.
#[derive(Debug)]
pub struct Ledger {
    dir: PathBuf,
    session: String,
    warned: AtomicBool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    session: String,
    model: String,
    usd: Option<f64>,
    tokens: TokenUsage,
}

impl Ledger {
    #[must_use]
    pub fn new(state_dir: &Path, session: &str) -> Self {
        Self {
            dir: state_dir.join("spend"),
            session: session.to_owned(),
            warned: AtomicBool::new(false),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir
            .join(format!("{}.jsonl", dino_core::memory::today()))
    }

    fn append(&self, model: &str, usd: Option<f64>, tokens: &TokenUsage) {
        let entry = Entry {
            session: self.session.clone(),
            model: model.to_owned(),
            usd,
            tokens: *tokens,
        };
        let written = std::fs::create_dir_all(&self.dir).and_then(|()| {
            let mut line = serde_json::to_string(&entry).map_err(std::io::Error::other)?;
            line.push('\n');
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path())?
                .write_all(line.as_bytes())
        });
        // Once: a full disk should not repeat itself on every response.
        if let Err(error) = written
            && !self.warned.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "dino: cannot keep today's spending in {}: {error}",
                self.dir.display()
            );
        }
    }

    /// A line that does not parse, such as one cut short by a crash, is skipped.
    fn today(&self) -> f64 {
        std::fs::read_to_string(self.path())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
            .filter_map(|entry| entry.usd)
            .sum()
    }
}

/// A session whose responses are priced as they arrive.
pub struct Metered<S> {
    inner: S,
    meter: Arc<Meter>,
}

impl<S> Metered<S> {
    pub fn new(inner: S, meter: Arc<Meter>) -> Self {
        Self { inner, meter }
    }
}

impl<S: ModelSession> ModelSession for Metered<S> {
    fn profile(&self) -> &ModelProfile {
        self.inner.profile()
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        Box::pin(async move {
            let stream = self.inner.next(input).await?;
            // Read after the call, which is when a fallback has chosen its model.
            let model = self.inner.profile().key.clone();
            let price = self.inner.profile().price;
            let meter = Arc::clone(&self.meter);
            let metered = stream.inspect(move |event| {
                if let Ok(ModelEvent::Usage { usage }) = event {
                    meter.record(&model, price, usage);
                }
            });
            Ok(Box::pin(metered) as ModelEventStream)
        })
    }

    fn discover_limits(&mut self) -> ModelFuture<'_, ()> {
        self.inner.discover_limits()
    }

    fn elide_tool_results(&mut self, keep_recent: usize) -> Result<Elision, ModelError> {
        self.inner.elide_tool_results(keep_recent)
    }

    fn export_state(&self) -> Result<SessionState, ModelError> {
        self.inner.export_state()
    }

    fn restore_state(&mut self, state: &SessionState) -> Result<(), ModelError> {
        self.inner.restore_state(state)
    }
}

/// Dollars to the cent once there are any, and to a hundredth of one before.
#[must_use]
pub fn usd(amount: f64) -> String {
    if amount >= 1.0 {
        format!("${amount:.2}")
    } else {
        format!("${amount:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cached: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            ..TokenUsage::default()
        }
    }

    fn state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dino-spend-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_session_adds_what_each_response_cost_and_counts_what_it_could_not_price() {
        let meter = Meter::new(None);
        let kimi = Pricing::new(2.0, 0.2, 10.0);

        meter.record(
            "kimi-k3.1-pro",
            Some(kimi),
            &usage(1_000_000, 900_000, 10_000),
        );
        meter.record("qwen3-8b", None, &usage(5_000, 0, 100));

        let spent = meter.spent();
        // 100k plain at 2.00, 900k cached at 0.20, 10k out at 10.00.
        assert!((spent.usd - 0.48).abs() < 1e-9, "{}", spent.usd);
        assert_eq!(spent.unpriced, 1);
        assert_eq!(meter.today(), None);
        assert!(
            meter.closing().contains("1 response(s)"),
            "{}",
            meter.closing()
        );
    }

    #[test]
    fn today_is_every_session_in_the_day_file() {
        let dir = state_dir("today");
        let price = Some(Pricing::new(2.0, 0.2, 10.0));
        let first = Meter::new(Some(Ledger::new(&dir, "one")));
        let second = Meter::new(Some(Ledger::new(&dir, "two")));

        first.record("kimi-k3.1-pro", price, &usage(1_000_000, 0, 0));
        second.record("kimi-k3.1-pro", price, &usage(0, 0, 100_000));
        second.record("qwen3-8b", None, &usage(1_000, 0, 0));
        let file = dir
            .join("spend")
            .join(format!("{}.jsonl", dino_core::memory::today()));
        let mut torn = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        torn.write_all(b"{\"session\":\"thr").unwrap();

        assert!((first.today().unwrap() - 3.0).abs() < 1e-9);
        assert!((second.spent().usd - 1.0).abs() < 1e-9);
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_amounts_keep_their_fractions_of_a_cent() {
        assert_eq!(usd(0.004_12), "$0.0041");
        assert_eq!(usd(12.345), "$12.35");
    }
}
