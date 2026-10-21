//! Whether a model that has stopped left the workspace compiling.

use gyr_core::CheckFuture;
use gyr_core::FinishCheck;

use crate::CargoTool;

/// How many errors are named to the model. The count says how many there are.
const NAMED_ERRORS: usize = 3;

/// Calls the work unfinished while `cargo check` reports errors.
pub struct BuildCheck {
    cargo: CargoTool,
}

impl BuildCheck {
    #[must_use]
    pub fn new(cargo: CargoTool) -> Self {
        Self { cargo }
    }
}

impl FinishCheck for BuildCheck {
    fn unfinished(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let parsed = self.cargo.parsed_check().await.ok()?;
            let errors = parsed.counts.errors;
            if errors == 0 {
                return None;
            }
            let named: Vec<String> = parsed
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.level == "error")
                .take(NAMED_ERRORS)
                .map(|diagnostic| match (&diagnostic.file, diagnostic.line) {
                    (Some(file), Some(line)) => format!("- {file}:{line}: {}", diagnostic.message),
                    _ => format!("- {}", diagnostic.message),
                })
                .collect();
            Some(format!(
                "[gyrfalcon: you ended your turn, but `cargo check` reports {errors} error(s), so \
                 the work is not finished. The first:\n{}\nFix them and check again. If the task \
                 cannot be done, call report_blocker and say why.]",
                named.join("\n")
            ))
        })
    }
}
