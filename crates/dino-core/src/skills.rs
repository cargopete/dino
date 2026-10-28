//! Skills: folders of instructions the model loads when one applies.
//!
//! Each is `<dir>/<name>/SKILL.md` with `description:` in its front matter, the
//! same shape Claude Code uses. Only names and descriptions go into the system
//! prompt; the body arrives through the `skill` tool when it is wanted.

use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;

use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::ToolError;
use crate::ToolFuture;
use crate::ToolRuntime;

pub const SKILL: &str = "skill";
const MAX_BODY_BYTES: usize = 32 * 1_024;
const MAX_DESCRIPTION_BYTES: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Every skill beneath `dirs`, earlier directories winning a name. A folder
/// without a `SKILL.md` or without a description is not a skill.
#[must_use]
pub fn discover(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut found: Vec<Skill> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path().join("SKILL.md");
                let text = std::fs::read_to_string(&path).ok()?;
                let folder = entry.file_name().into_string().ok()?;
                let (front, _) = split_front_matter(&text);
                let field = |key: &str| {
                    front.lines().find_map(|line| {
                        line.strip_prefix(key)
                            .and_then(|rest| rest.strip_prefix(':'))
                            .map(|value| value.trim().trim_matches('"').to_owned())
                    })
                };
                let description = field("description").filter(|value| !value.is_empty())?;
                Some(Skill {
                    name: field("name")
                        .filter(|value| !value.is_empty())
                        .unwrap_or(folder),
                    description: cap(&description, MAX_DESCRIPTION_BYTES),
                    path,
                })
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in found {
            if !skills.iter().any(|known| known.name == skill.name) {
                skills.push(skill);
            }
        }
    }
    skills
}

/// The front matter between leading `---` lines, and the body after it.
fn split_front_matter(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---") else {
        return ("", text);
    };
    match rest.find("\n---") {
        Some(end) => {
            let body = &rest[end + 4..];
            (&rest[..end], body.strip_prefix('\n').unwrap_or(body))
        }
        None => ("", text),
    }
}

fn cap(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[derive(Debug, Clone)]
pub struct SkillTool {
    skills: Vec<Skill>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    name: String,
}

impl SkillTool {
    #[must_use]
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }

    fn find(&self, arguments: &Value) -> Result<&Skill, ToolError> {
        let request: Request = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::new(format!("invalid {SKILL} arguments: {error}")))?;
        self.skills
            .iter()
            .find(|skill| skill.name == request.name)
            .ok_or_else(|| ToolError::new(format!("no skill named {:?}", request.name)))
    }
}

/// The body, and the other files in the skill's folder, which its text often
/// names as scripts or references to read.
fn load(skill: &Skill) -> Result<String, ToolError> {
    let text = std::fs::read_to_string(&skill.path).map_err(|error| {
        ToolError::new(format!("cannot read {}: {error}", skill.path.display()))
    })?;
    let (_, body) = split_front_matter(&text);
    let mut output = cap(body.trim(), MAX_BODY_BYTES);
    let folder = skill.path.parent().unwrap_or(Path::new("."));
    let mut files = Vec::new();
    collect(folder, folder, &mut files);
    files.retain(|file| file != "SKILL.md");
    files.sort();
    if !files.is_empty() {
        let _ = write!(
            &mut output,
            "\n\nFiles in this skill, beneath {}:\n",
            folder.display()
        );
        for file in files.iter().take(50) {
            let _ = writeln!(&mut output, "- {file}");
        }
    }
    Ok(output)
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.display().to_string());
        }
    }
}

impl ToolRuntime for SkillTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: SKILL.into(),
            description: "Load one of the skills listed in the system prompt, by name. Load a \
                          skill as soon as its description fits the task, and follow it."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "enum": self.skills.iter().map(|skill| skill.name.clone()).collect::<Vec<_>>()
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != SKILL {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        self.find(&call.arguments)?;
        Ok(ToolAction::read_only())
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let result = self
            .find(&call.arguments)
            .and_then(load)
            .map(ToolOutput::success);
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("SKILL.md"), text).unwrap();
    }

    #[tokio::test]
    async fn skills_are_found_listed_and_loaded_with_their_files() {
        let base = std::env::temp_dir().join(format!("dino-skills-{}", std::process::id()));
        let user = base.join("user");
        let project = base.join("project");
        write(
            &user,
            "release",
            "---\nname: release\ndescription: Cut a release. Use when asked to ship.\n---\n\n# Release\nRun scripts/tag.sh.\n",
        );
        std::fs::create_dir_all(user.join("release/scripts")).unwrap();
        std::fs::write(user.join("release/scripts/tag.sh"), "#!/bin/sh\n").unwrap();
        write(
            &project,
            "release",
            "---\ndescription: The project's own.\n---\nbody\n",
        );
        write(
            &project,
            "review",
            "---\ndescription: \"Review a diff.\"\n---\nLook hard.\n",
        );
        write(&project, "nodesc", "---\nname: nodesc\n---\nbody\n");

        let skills = discover(&[user.clone(), project.clone()]);

        let listed: Vec<_> = skills
            .iter()
            .map(|skill| (skill.name.as_str(), skill.description.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("release", "Cut a release. Use when asked to ship."),
                ("review", "Review a diff."),
            ]
        );

        let tool = SkillTool::new(skills);
        let output = tool
            .execute(&ToolCall {
                id: "call-1".into(),
                name: SKILL.into(),
                arguments: json!({"name": "release"}),
            })
            .await
            .unwrap();
        assert!(
            output.content.starts_with("# Release\nRun scripts/tag.sh."),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("- scripts/tag.sh"),
            "{}",
            output.content
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
