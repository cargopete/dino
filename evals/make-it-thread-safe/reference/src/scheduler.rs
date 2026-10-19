use std::sync::atomic::{AtomicU64, Ordering};

use crate::registry::Registry;

/// Drives every running job in a registry forward, one step per tick.
pub struct Scheduler {
    registry: Registry,
    ticks: AtomicU64,
}

impl Scheduler {
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            ticks: AtomicU64::new(0),
        }
    }

    /// Advances each running job once and returns how many jobs it actually
    /// moved forward.
    pub fn tick(&self) -> usize {
        self.ticks.fetch_add(1, Ordering::Relaxed);
        let mut advanced = 0;
        for id in self.registry.running() {
            if let Some((_, true)) = self.registry.step(id) {
                advanced += 1;
            }
        }
        advanced
    }

    /// Ticks until nothing is left running and returns the number of steps
    /// taken across all jobs.
    pub fn run_until_idle(&self) -> usize {
        let mut steps = 0;
        loop {
            let advanced = self.tick();
            if advanced == 0 {
                return steps;
            }
            steps += advanced;
        }
    }

    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
}
