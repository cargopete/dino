//! The `packages` tool: fetching npm, pnpm and Go dependencies, the one way
//! past a sandbox that otherwise denies the network.
//!
//! The fetch runs with HTTPS out but writes still confined to the workspace,
//! and with nothing executed that a package supplies: npm and pnpm get
//! `--ignore-scripts`, and Go reaches only its module proxy, never a VCS.

use std::sync::Arc;
use std::time::Duration;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::workspace::WorkspaceRoot;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Sandbox;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::exec::ExecLimits;
use crate::exec::ExecOutput;
use crate::exec::capped;
use crate::exec::json_output;
use crate::process;

const NAME: &str = "packages";
const FETCH_NOTE: &str = " (network: HTTPS to the package registry)";

#[derive(Debug)]
pub struct PackagesTool {
    root: WorkspaceRoot,
    limits: ExecLimits,
    sandbox: Arc<dyn Sandbox>,
}

impl PackagesTool {
    /// Creates the tool for one workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the root cannot be canonicalised or is not a
    /// directory.
    pub fn new(
        root: impl AsRef<std::path::Path>,
        limits: ExecLimits,
        sandbox: Arc<dyn Sandbox>,
    ) -> Result<Self, ToolError> {
        Ok(Self {
            root: WorkspaceRoot::new(root)?,
            limits,
            sandbox,
        })
    }

    async fn run(&self, request: Request) -> Result<ToolOutput, ToolError> {
        let command = request.command()?;
        let sandbox = match self.sandbox.cache_dir() {
            Some(cache) => {
                std::fs::create_dir_all(&cache).map_err(|error| {
                    ToolError::new(format!("cannot create {}: {error}", cache.display()))
                })?;
                self.sandbox.for_fetching(&[cache])
            }
            None => self.sandbox.for_fetching(&[]),
        }
        .ok_or_else(|| {
            ToolError::new(format!(
                "this sandbox ({}) cannot allow fetching; ask the person to run {} themselves",
                self.sandbox.label(),
                command.join(" ")
            ))
        })?;
        let (program, arguments) = command
            .split_first()
            .ok_or_else(|| ToolError::new("no command"))?;
        // Proxy only: `direct` would clone from a VCS, which runs git with the
        // network open and trusts whatever host a module path names.
        let extra: &[(&str, &str)] = if request.manager == Manager::Go {
            &[
                ("GOPROXY", "https://proxy.golang.org"),
                ("GOFLAGS", "-mod=mod"),
            ]
        } else {
            &[]
        };
        let execution = process::run_with(
            program,
            arguments,
            self.root.path(),
            sandbox.as_ref(),
            self.limits.max_captured_bytes,
            self.limits.timeout.max(Duration::from_secs(600)),
            extra,
        )
        .await?;
        let max = self.limits.max_output_bytes;
        let stderr = if execution.timed_out {
            "no result: killed before it finished".to_owned()
        } else {
            capped(&execution.stderr, max)
        };
        json_output(&ExecOutput {
            command: command.join(" "),
            exit_code: execution.exit_code,
            timed_out: execution.timed_out,
            stdout: capped(&execution.stdout, max),
            stderr,
            truncated: execution.truncated
                || execution.stdout.len() > max
                || execution.stderr.len() > max,
            restored_protected_paths: execution.restored,
            note: None,
        })
    }
}

impl ToolRuntime for PackagesTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: NAME.into(),
            description: "Download npm, pnpm or Go dependencies: the only way to reach a \
                 package registry, since other commands have no network. `install` fetches \
                 what package.json or go.mod already declares; `add` adds packages and fetches \
                 them. Package install scripts never run. For Rust, use the cargo tool."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "manager": {"type": "string", "enum": ["npm", "pnpm", "go"]},
                    "command": {
                        "type": "string",
                        "enum": ["install", "add", "tidy"],
                        "description": "tidy is Go only: go mod tidy."
                    },
                    "packages": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "For add: registry names, optionally with a version, such as react@19 or github.com/google/uuid@v1.6.0."
                    },
                    "dev": {"type": "boolean", "description": "For npm and pnpm add: a dev dependency."}
                },
                "required": ["manager", "command"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != NAME {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        let request = Request::parse(&call.arguments)?;
        Ok(ToolAction::process(
            request.command()?.join(" ") + FETCH_NOTE,
        ))
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let request = if call.name == NAME {
            Request::parse(&call.arguments)
        } else {
            Err(ToolError::new(format!("unknown tool {:?}", call.name)))
        };
        Box::pin(async move { self.run(request?).await })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Manager {
    Npm,
    Pnpm,
    Go,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Command {
    Install,
    Add,
    Tidy,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    manager: Manager,
    command: Command,
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    dev: bool,
}

impl Request {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        if arguments.get("manager").and_then(Value::as_str) == Some("cargo") {
            return Err(ToolError::new(
                "packages does not handle Rust: use the cargo tool's add or fetch command.",
            ));
        }
        serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::new(format!("invalid packages arguments: {error}")))
    }

    /// The command line, after checking every package is a registry name.
    fn command(&self) -> Result<Vec<String>, ToolError> {
        for package in &self.packages {
            check_package(package)?;
        }
        let owned = |parts: &[&str]| {
            parts
                .iter()
                .map(|part| (*part).to_owned())
                .collect::<Vec<_>>()
        };
        let mut command = match (self.manager, self.command) {
            (_, Command::Add) if self.packages.is_empty() => {
                return Err(ToolError::new("add needs at least one package"));
            }
            (_, Command::Install | Command::Tidy) if !self.packages.is_empty() => {
                return Err(ToolError::new(
                    "install and tidy fetch what is declared; use add to name packages",
                ));
            }
            (Manager::Npm | Manager::Pnpm, Command::Tidy) => {
                return Err(ToolError::new("tidy is for Go; npm and pnpm use install"));
            }
            (Manager::Npm, Command::Install) => owned(&[
                "npm",
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
            ]),
            (Manager::Npm, Command::Add) => {
                let mut command = owned(&[
                    "npm",
                    "install",
                    "--ignore-scripts",
                    "--no-audit",
                    "--no-fund",
                ]);
                if self.dev {
                    command.push("--save-dev".to_owned());
                }
                command
            }
            (Manager::Pnpm, Command::Install) => owned(&["pnpm", "install", "--ignore-scripts"]),
            (Manager::Pnpm, Command::Add) => {
                let mut command = owned(&["pnpm", "add", "--ignore-scripts"]);
                if self.dev {
                    command.push("--save-dev".to_owned());
                }
                command
            }
            (Manager::Go, Command::Install) => owned(&["go", "mod", "download"]),
            (Manager::Go, Command::Add) => owned(&["go", "get"]),
            (Manager::Go, Command::Tidy) => owned(&["go", "mod", "tidy"]),
        };
        if self.command == Command::Add {
            command.extend(self.packages.iter().cloned());
        }
        Ok(command)
    }
}

/// A registry package name with an optional version, and nothing that would
/// make the manager fetch from somewhere else or read it as a flag.
fn check_package(package: &str) -> Result<(), ToolError> {
    const ELSEWHERE: &[&str] = &[
        "file:",
        "link:",
        "git+",
        "git:",
        "git@",
        "http:",
        "https:",
        "ssh:",
        "github:",
        "gitlab:",
        "bitbucket:",
        "npm:",
        "workspace:",
        "portal:",
        "patch:",
    ];
    let refused = package.is_empty()
        || package.starts_with('-')
        || package.starts_with('.')
        || package.starts_with('/')
        || package.contains(char::is_whitespace)
        || package.contains("://")
        || ELSEWHERE.iter().any(|prefix| package.starts_with(prefix));
    if refused {
        return Err(ToolError::new(format!(
            "{package:?} is not a registry package name; only names from the npm registry or \
             the Go module proxy can be fetched here"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(arguments: &Value) -> Result<Vec<String>, ToolError> {
        Request::parse(arguments)?.command()
    }

    #[test]
    fn installs_never_run_package_scripts() {
        let npm = command(
            &json!({"manager": "npm", "command": "add", "packages": ["react@19"], "dev": true}),
        )
        .unwrap();
        assert_eq!(
            npm,
            [
                "npm",
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--save-dev",
                "react@19"
            ]
        );
        let pnpm = command(&json!({"manager": "pnpm", "command": "install"})).unwrap();
        assert!(pnpm.contains(&"--ignore-scripts".to_owned()));
        let go = command(&json!({"manager": "go", "command": "add", "packages": ["github.com/google/uuid@v1.6.0"]})).unwrap();
        assert_eq!(go, ["go", "get", "github.com/google/uuid@v1.6.0"]);
    }

    #[test]
    fn a_rust_request_is_pointed_at_the_cargo_tool() {
        let error = command(&json!({"manager": "cargo", "command": "add", "packages": ["serde"]}))
            .unwrap_err();
        assert!(error.to_string().contains("cargo tool"), "{error}");
    }

    #[test]
    fn only_registry_names_are_fetched() {
        for bad in [
            "--registry=https://evil.example",
            "git+ssh://x/y",
            "github:user/repo",
            "file:../x",
            "https://x/y.tgz",
            "./local",
            "a b",
        ] {
            let error = command(&json!({"manager": "npm", "command": "add", "packages": [bad]}))
                .unwrap_err();
            assert!(error.to_string().contains("registry"), "{bad}: {error}");
        }
        for good in ["@types/node@22", "git-url-parse", "react@^19.1.0"] {
            assert!(
                command(&json!({"manager": "npm", "command": "add", "packages": [good]})).is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn commands_and_managers_must_agree() {
        assert!(command(&json!({"manager": "npm", "command": "tidy"})).is_err());
        assert!(command(&json!({"manager": "go", "command": "add"})).is_err());
        assert!(
            command(&json!({"manager": "npm", "command": "install", "packages": ["x"]})).is_err()
        );
        assert!(command(&json!({"manager": "yarn", "command": "install"})).is_err());
    }
}
