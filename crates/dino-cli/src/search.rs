//! Web search: the provider's own where it runs one, else the engine named in
//! the user file, and off in `minimal` and evals.
//!
//! The provider's search runs inside its response, so no call reaches the
//! approval policy before it happens. It is offered only once the session has
//! been allowed it: the first request asks, as for any network call, and the
//! answer holds for every request and model after it.

use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Result;
use anyhow::bail;
use dino_core::approval::ApprovalPolicy;
use dino_model::Elision;
use dino_model::ModelError;
use dino_model::ModelEventStream;
use dino_model::ModelFuture;
use dino_model::ModelSession;
use dino_model::NativeSearch;
use dino_model::SessionState;
use dino_protocol::ApprovalDecision;
use dino_protocol::ModelProfile;
use dino_protocol::ProviderKind;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::TurnInput;

use crate::settings::SearchBackendConfig;

/// How a session searches, if it does.
#[derive(Debug, Clone, Default)]
pub enum WebSearch {
    #[default]
    Off,
    /// At the provider, once the gate has let it.
    Native(Arc<Gate>),
    /// Through `web_search`, a tool like any other.
    Backend(dino_exec::SearchBackend),
}

impl WebSearch {
    /// What the session's search is, for the banner and `dino config`.
    pub fn label(&self) -> String {
        match self {
            Self::Off => "off".to_owned(),
            Self::Native(gate) => format!("at the provider ({}), asked once", gate.host),
            Self::Backend(backend) => {
                format!(
                    "{} ({}), asked once",
                    backend.engine().name(),
                    backend.host()
                )
            }
        }
    }

    pub fn backend(&self) -> Option<dino_exec::SearchBackend> {
        match self {
            Self::Backend(backend) => Some(backend.clone()),
            Self::Off | Self::Native(_) => None,
        }
    }

    /// The switch for a session on `provider`. Only the provider that was
    /// asked about gets one: a fallback or a `/model` change elsewhere would
    /// send queries somewhere the person never allowed.
    pub fn switch(&self, provider: ProviderKind) -> Option<NativeSearch> {
        match self {
            Self::Native(gate) if native_host(provider) == Some(gate.host) => {
                Some(gate.switch.clone())
            }
            Self::Off | Self::Native(_) | Self::Backend(_) => None,
        }
    }
}

/// Where a provider's own search is asked about: its API host.
fn native_host(provider: ProviderKind) -> Option<&'static str> {
    match provider {
        ProviderKind::Anthropic => Some("api.anthropic.com"),
        ProviderKind::OpenAi => Some("api.openai.com"),
        ProviderKind::Qwen => None,
    }
}

/// Decides how a session on `profile` searches.
///
/// # Errors
///
/// Returns an error when the configured engine is unusable or its key's
/// variable is unset.
pub fn resolve(
    profile: &ModelProfile,
    enabled: bool,
    backend: Option<&SearchBackendConfig>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<WebSearch> {
    if !enabled {
        return Ok(WebSearch::Off);
    }
    if let Some(host) = native_host(profile.provider) {
        return Ok(WebSearch::Native(Arc::new(Gate::new(host))));
    }
    let Some(config) = backend else {
        return Ok(WebSearch::Off);
    };
    let key = match &config.key_env {
        Some(name) => match env(name) {
            Some(key) if !key.is_empty() => Some(key),
            _ => bail!("search_backend.key_env names {name}, which is not set"),
        },
        None => None,
    };
    let backend = dino_exec::SearchBackend::new(config.engine, config.url.as_deref(), key)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(WebSearch::Backend(backend))
}

/// Whether the provider's search may run this session, asked once.
pub struct Gate {
    host: &'static str,
    switch: NativeSearch,
    policy: OnceLock<Box<dyn ApprovalPolicy>>,
    decided: tokio::sync::OnceCell<bool>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("host", &self.host)
            .field("on", &self.switch.on())
            .finish_non_exhaustive()
    }
}

impl Gate {
    fn new(host: &'static str) -> Self {
        Self {
            host,
            switch: NativeSearch::default(),
            policy: OnceLock::new(),
            decided: tokio::sync::OnceCell::new(),
        }
    }

    /// Who decides. Until this is called the search stays off.
    pub fn decide_with(&self, policy: Box<dyn ApprovalPolicy>) {
        let _ = self.policy.set(policy);
    }

    /// The call the policy is asked about: the same tool and host a rule for
    /// it is kept under.
    fn call(&self) -> (ToolCall, ToolAction) {
        let call = ToolCall {
            id: "web-search-at-provider".to_owned(),
            name: dino_exec::WEB_SEARCH.to_owned(),
            arguments: serde_json::json!({ "query": "(this session's searches, at the provider)" }),
        };
        (call, ToolAction::network(self.host))
    }

    async fn allowed(&self) -> bool {
        *self
            .decided
            .get_or_init(|| async {
                let Some(policy) = self.policy.get() else {
                    return false;
                };
                let (call, action) = self.call();
                let allowed = matches!(
                    policy.decide(&call, &action).await,
                    ApprovalDecision::Allowed { .. }
                );
                self.switch.set(allowed);
                allowed
            })
            .await
    }
}

/// A session whose first request waits for the gate.
pub struct Gated<S> {
    inner: S,
    gate: Arc<Gate>,
}

impl<S> Gated<S> {
    pub fn new(inner: S, gate: Arc<Gate>) -> Self {
        Self { inner, gate }
    }
}

impl<S: ModelSession> ModelSession for Gated<S> {
    fn profile(&self) -> &ModelProfile {
        self.inner.profile()
    }

    fn next(&mut self, input: TurnInput) -> ModelFuture<'_, ModelEventStream> {
        Box::pin(async move {
            self.gate.allowed().await;
            self.inner.next(input).await
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

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(key: &str) -> ModelProfile {
        dino_model::builtin_profile(key).unwrap()
    }

    #[test]
    fn a_provider_with_its_own_search_uses_it_and_one_without_uses_the_engine() {
        let none = |_: &str| None;
        let brave = SearchBackendConfig {
            engine: dino_exec::SearchEngine::Brave,
            url: None,
            key_env: Some("BRAVE_KEY".into()),
        };
        let key = |name: &str| (name == "BRAVE_KEY").then(|| "k".to_owned());

        assert!(matches!(
            resolve(&profile("claude-opus"), true, Some(&brave), &key).unwrap(),
            WebSearch::Native(_)
        ));
        assert!(matches!(
            resolve(&profile("kimi-k3.1-pro"), true, Some(&brave), &key).unwrap(),
            WebSearch::Backend(_)
        ));
        assert!(matches!(
            resolve(&profile("kimi-k3.1-pro"), true, None, &none).unwrap(),
            WebSearch::Off
        ));
        assert!(matches!(
            resolve(&profile("claude-opus"), false, None, &none).unwrap(),
            WebSearch::Off
        ));
        let native = resolve(&profile("claude-opus"), true, None, &none).unwrap();
        assert!(native.switch(ProviderKind::Anthropic).is_some());
        assert!(native.switch(ProviderKind::OpenAi).is_none());
        let error = resolve(&profile("kimi-k3.1-pro"), true, Some(&brave), &none).unwrap_err();
        assert!(error.to_string().contains("BRAVE_KEY"), "{error}");
    }

    struct Answer(bool, Arc<std::sync::atomic::AtomicUsize>);

    impl ApprovalPolicy for Answer {
        fn decide(
            &self,
            call: &ToolCall,
            action: &ToolAction,
        ) -> dino_core::approval::DecisionFuture<'_> {
            assert_eq!(call.name, "web_search");
            assert_eq!(action.subject.as_deref(), Some("api.anthropic.com"));
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let decision = if self.0 {
                ApprovalDecision::allowed(dino_protocol::DecisionSource::User)
            } else {
                ApprovalDecision::denied("no")
            };
            Box::pin(async move { decision })
        }
    }

    #[tokio::test]
    async fn the_gate_asks_once_and_switches_the_search_on_only_when_allowed() {
        assert!(!Gate::new("api.anthropic.com").allowed().await);
        for allow in [true, false] {
            let gate = Gate::new("api.anthropic.com");
            let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            gate.decide_with(Box::new(Answer(allow, Arc::clone(&asked))));
            assert_eq!(gate.allowed().await, allow);
            assert_eq!(gate.allowed().await, allow);
            assert_eq!(gate.switch.on(), allow);
            assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 1);
        }
    }
}
