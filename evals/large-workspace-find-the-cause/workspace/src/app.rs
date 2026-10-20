use crate::config::Config;
use crate::error::Error;
use crate::model::ledger::Ledger;
use crate::parse;
use crate::plugins::{PluginRegistry, RenderContext};
use crate::report::detail::Detail;
use crate::report::summary::Summary;

/// A loaded config and log, ready to report on.
pub struct App {
    config: Config,
    ledger: Ledger,
    plugins: PluginRegistry,
}

impl App {
    /// Builds an app from the text of a config file and of a log file.
    pub fn from_sources(config: &str, log: &str) -> Result<Self, Error> {
        let config = Config::from_text(config)?;
        let entries = parse::parse_log(log, &config)?;
        let mut ledger = Ledger::new(entries);
        if let Some(period) = config.filter.period {
            ledger = ledger.within(period);
        }
        Ok(Self {
            config,
            ledger,
            plugins: PluginRegistry::with_builtins(),
        })
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The summary in the format the config names, `plain` unless it says
    /// otherwise.
    pub fn summary(&self) -> Result<String, Error> {
        self.export(&self.config.report.format)
    }

    /// The summary rendered by the named plugin.
    pub fn export(&self, plugin: &str) -> Result<String, Error> {
        let plugin = self
            .plugins
            .get(plugin)
            .ok_or_else(|| Error::UnknownPlugin(plugin.to_owned()))?;
        let summary = Summary::build(&self.ledger, &self.config);
        Ok(plugin.render(&summary, &RenderContext::from_config(&self.config)))
    }

    /// Every entry for one project, with its sessions and total.
    pub fn detail(&self, project: &str) -> Result<String, Error> {
        let detail = Detail::build(&self.ledger, project)
            .ok_or_else(|| Error::UnknownProject(project.to_owned()))?;
        Ok(detail.render())
    }
}
