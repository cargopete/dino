use std::collections::BTreeMap;

use crate::plugins::csv::CsvPlugin;
use crate::plugins::json::JsonPlugin;
use crate::plugins::markdown::MarkdownPlugin;
use crate::plugins::plain::PlainPlugin;
use crate::plugins::ReportPlugin;

#[derive(Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<&'static str, Box<dyn ReportPlugin>>,
}

impl PluginRegistry {
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(Box::new(PlainPlugin));
        registry.register(Box::new(CsvPlugin));
        registry.register(Box::new(MarkdownPlugin));
        registry.register(Box::new(JsonPlugin));
        registry
    }

    /// Adds a plugin, replacing any registered under the same name.
    pub fn register(&mut self, plugin: Box<dyn ReportPlugin>) {
        self.plugins.insert(plugin.name(), plugin);
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn ReportPlugin> {
        self.plugins.get(name).map(|plugin| plugin.as_ref())
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.plugins.keys().copied()
    }
}
