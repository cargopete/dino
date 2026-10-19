use std::cell::Cell;

use crate::registry::Registry;

/// Drives every running job in a registry forward, one step per tick.
pub struct Scheduler {
    registry: Registry,
    ticks: Cell<u64>,
}

impl Scheduler {
    #[must_use]
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            ticks: Cell::new(0),
        }
    }

    /// Advances each running job once and returns how many jobs it actually
    /// moved forward.
    pub fn tick(&self) -> usize {
        self.ticks.set(self.ticks.get() + 1);
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
        self.ticks.get()
    }

    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
}
