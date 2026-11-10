//! Named presets: the tools a session offers, and the limits it starts under.
//!
//! A preset only narrows. Whatever it leaves out behaves as a tool never built,
//! and an explicit setting of turns still wins over the preset's.

use std::num::NonZeroU32;

use crate::config::ApprovalMode;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[clap(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Every tool, MCP servers and, in a session, schedules.
    #[default]
    Standard,
    /// Files, processes and cargo: a short tool list for a small model.
    Minimal,
    /// Reading only, with read-only approvals and fewer turns.
    Review,
}

const MINIMAL: &[&str] = &[
    "read",
    "search",
    "list",
    "apply_patch",
    "write_file",
    "exec",
    "shell",
    "cargo",
    dino_core::blocker::REPORT_BLOCKER,
];

const REVIEW: &[&str] = &[
    "read",
    "search",
    "list",
    "github",
    "web_fetch",
    dino_core::skills::SKILL,
    dino_core::todo::TODO,
    dino_core::blocker::REPORT_BLOCKER,
];

impl Preset {
    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Minimal => "minimal",
            Self::Review => "review",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "standard (every tool)",
            Self::Minimal => "minimal (files, processes and cargo)",
            Self::Review => "review (reading only, read-only approvals)",
        }
    }

    /// Whether the session offers the tool named. A mode's own tool, such as a
    /// handover's progress record, is always kept: the mode cannot run without it.
    pub fn offers(self, tool: &str) -> bool {
        let named = match self {
            Self::Standard => return true,
            Self::Minimal => MINIMAL,
            Self::Review => REVIEW,
        };
        named.contains(&tool) || tool == dino_core::progress::PROGRESS
    }

    /// Whether MCP servers are started at all. Their tools are unknown ahead,
    /// so a preset with a fixed list leaves them out.
    pub fn connects_mcp(self) -> bool {
        self == Self::Standard
    }

    /// The approvals the preset requires, overriding a looser one from a file.
    pub fn approvals(self) -> Option<ApprovalMode> {
        (self == Self::Review).then_some(ApprovalMode::ReadOnly)
    }

    /// Model turns per submission when neither a flag nor a file sets them.
    pub fn max_turns(self) -> Option<NonZeroU32> {
        match self {
            Self::Review => NonZeroU32::new(40),
            Self::Standard | Self::Minimal => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_offers_everything_and_the_others_their_lists() {
        assert!(Preset::Standard.offers("schedule_create"));
        assert!(Preset::Minimal.offers("apply_patch"));
        assert!(!Preset::Minimal.offers("web_fetch"));
        assert!(!Preset::Minimal.offers("web_search"));
        assert!(!Preset::Minimal.offers("ask"));
        assert!(Preset::Review.offers("read"));
        assert!(!Preset::Review.offers("write_file"));
        assert!(!Preset::Review.offers("shell"));
    }

    #[test]
    fn no_preset_but_standard_offers_schedules() {
        for preset in [Preset::Minimal, Preset::Review] {
            for name in dino_core::schedule::NAMES {
                assert!(!preset.offers(name), "{preset:?} offers {name}");
            }
        }
    }

    #[test]
    fn a_handover_keeps_its_progress_record_under_any_preset() {
        assert!(Preset::Minimal.offers(dino_core::progress::PROGRESS));
        assert!(Preset::Review.offers(dino_core::progress::PROGRESS));
    }

    #[test]
    fn only_review_sets_approvals_and_turns() {
        assert_eq!(Preset::Review.approvals(), Some(ApprovalMode::ReadOnly));
        assert_eq!(Preset::Minimal.approvals(), None);
        assert_eq!(Preset::Review.max_turns(), NonZeroU32::new(40));
        assert_eq!(Preset::Standard.max_turns(), None);
    }
}
